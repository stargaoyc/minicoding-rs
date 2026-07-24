# 数据模型与存储设计

本文描述 `minicoding-rs` 的数据模型、序列化格式、持久化策略、会话日志结构、记忆存储与索引设计。

---

## 1. 数据模型总览

```
Session
 ├── id: SessionId
 ├── metadata: SessionMeta
 └── messages: Vec<Message>
                  ├── role
                  ├── content: Vec<ContentBlock>
                  ├── tool_calls: Vec<ToolCall>
                  └── metadata: MessageMeta

ContextSnapshot          (ContextManager 运行时镜像)
 ├── messages
 ├── token_count
 └── compression_log: Vec<CompressionStep>

PermissionStore          (决策持久化)
 └── rules: Vec<Rule>

MemoryStore
 ├── long_term: MarkdownFile
 └── sessions/: Vec<SessionSummary>
```

---

## 2. 消息序列化格式

### 2.1 设计目标

- **人可读**：便于调试与回放；
- **追加友好**：JSONL，每行独立，崩溃不破坏已写部分；
- **前向兼容**：新增字段不破坏旧文件；
- **可索引**：每条记录带稳定 `id` 与时间戳。

### 2.2 JSONL 记录结构

会话文件 `~/.minicoding/sessions/{session_id}.jsonl`，每行一条记录。`message` 行新增可选字段 `parent_uuid`（与 `design.md` §10.3 对齐），用于支持 Fork / 压缩边界 / Side-chain。**默认情况下 `parent_uuid` 等于上一行 `message` 的 `id`，线性读取不需要建 DAG**——`parent_uuid` 仅在 Fork/Side-chain 检视时被使用。

```json
{"v":1,"type":"session_start","id":"sess_01H...","created_at":"2026-07-24T10:00:00Z","workdir":"e:/projects/foo","config_hash":1234567890,"provider":"anthropic","model":"claude-sonnet-4"}
{"v":1,"type":"message","id":"msg_01H...","parent_uuid":null,"role":"system","content":[{"type":"text","text":"You are..."}],"created_at":"...","meta":{"tokens":42,"source":"system"}}
{"v":1,"type":"message","id":"msg_01H...","parent_uuid":"msg_01H...","role":"user","content":[{"type":"text","text":"解释入口"}],"created_at":"...","meta":{"source":"user"}}
{"v":1,"type":"message","id":"msg_01H...","parent_uuid":"msg_01H...","role":"assistant","content":[{"type":"text","text":"让我读取"}],"tool_calls":[{"id":"call_1","name":"fs.read","input":{"path":"src/main.rs"}}],"created_at":"...","meta":{"tokens":28,"source":"llm","usage":{"input":512,"output":28}}}
{"v":1,"type":"message","id":"msg_01H...","parent_uuid":"msg_01H...","role":"tool","tool_call_id":"call_1","content":[{"type":"tool_result","call_id":"call_1","content":{"type":"text","text":"fn main() {...}"},"is_error":false}],"created_at":"...","meta":{"source":"tool","tool_name":"fs.read","elapsed_ms":3,"bytes":1024}}
{"v":1,"type":"compression","id":"cmp_01H...","at":"...","steps":[{"kind":"tool_result_truncate","affected":["msg_01H..."]},{"kind":"summarize","affected":["msg_01H...","msg_01H..."],"summary_id":"msg_01H..."}],"tokens_before":12000,"tokens_after":4500}
{"v":1,"type":"message","id":"msg_01H...","parent_uuid":null,"role":"system","content":[{"type":"text","text":"[summarized] ..."}],"created_at":"...","meta":{"source":"summarize","summarized":true}}
{"v":1,"type":"session_end","at":"...","reason":"normal"}
```

### 2.3 字段说明

| 字段 | 说明 |
|------|------|
| `v` | schema 版本，当前 1 |
| `type` | `session_start` / `message` / `compression` / `session_end` / `permission` / `error` |
| `id` | ULID，全局唯一且时间有序 |
| `parent_uuid` | 仅 `message` 行：父消息 `id`。默认 = 上一行 `message` 的 `id`；`null` 表示链头或压缩边界（摘要行）；side-chain 头指向派发它的 `task.spawn` 工具调用 `id`。**可选字段，旧文件读取时按 `None` 处理并线性重建**（`#[serde(default)]`） |
| `meta.tokens` | 该消息占用 token 数（assistant 含 output） |
| `meta.usage` | 仅 assistant：上游返回的 token 用量 |
| `meta.source` | 消息产生方：`system`/`user`/`llm`/`tool`/`subagent`/`summarize` |
| `meta.pinned` | 是否用户固定（不被压缩） |
| `meta.summarized` | 是否为摘要替换后的消息 |

### 2.4 兼容性策略

- 读取时忽略未知字段（`#[serde(default)]` + `serde_json::Value` 兜底）。
- `v` 字段用于 schema 迁移：`migrate(v_from, v_to, record)` 链式升级。
- 旧字段保留至少 2 个大版本，标注 deprecated。
- **`parent_uuid` 前向兼容**：旧文件（v=1，无 `parent_uuid` 字段）读取时 `parent_uuid` 默认 `None`，`Storage::load` 线性扫描时按"上一行 `id`"自动回填，等价于纯数组顺序模型——旧文件零迁移可用。

---

## 3. 存储分层

### 3.0 路径约定（权威，全局生效）

`minicoding-rs` 采用**单根目录**约定，所有持久化数据集中在同一根下，避免 XDG 多目录分散带来的路径漂移：

- 根目录默认为 `~/.minicoding/`。
- 可通过环境变量 `MINICODING_HOME` 覆盖（绝对路径）；设置后所有子路径都相对该根解析。
- 项目级配置仍使用工作目录下的 `.minicoding.toml`（见 `architecture.md` §7.1）。
- 凭证**不**落入此目录，统一存 OS keyring（见 §7）。

> 全项目所有文档、代码、日志中出现的 `~/.minicoding/...` 路径，均指"根目录"下的相对位置，根目录由上述规则确定。`api.md`、`architecture.md`、`security.md` 等引用路径时以此为准。

```
$MINICODING_HOME  (默认 ~/.minicoding/)
├── config.toml              # 用户配置
├── policy.toml              # 权限决策持久化
├── audit.log                # 工具调用审计（JSONL，追加写）
├── AGENTS.md                # 全局项目记忆指令层（见 §6.4）
├── AGENTS.override.md       # 全局 override（可选，见 §6.4）
├── mcp.json                 # MCP server 配置（local + user 作用域，见 design.md §19.4）
├── mcp_choices.toml         # project 作用域 MCP server 批准记忆（见 design.md §19.4）
├── memory/
│   ├── long_term.md         # 长期记忆（人机共读，见 §6.1）
│   ├── long_term.index.json # 长期记忆索引（程序化查询，见 §6.1）
│   └── sessions/
│       └── {summary_id}.md  # 每会话摘要
├── sessions/
│   ├── index.json           # 会话索引（轻量元数据）
│   └── {session_id}.jsonl   # 会话日志（追加写）
└── logs/
    └── minicoding.YYYY-MM-DD.log
```

`.minicoding.toml`（项目级）与 `~/.minicoding/config.toml`（用户级）的合并优先级见 `architecture.md` §7.1。

### 3.1 会话索引 `index.json`

避免遍历所有 jsonl 即可列出会话，并缓存压缩边界指针以 O(1) 跳过已压缩前缀（与 `design.md` §10.4 协同）：

```json
{
  "v": 1,
  "sessions": [
    {
      "id": "sess_01H...",
      "created_at": "2026-07-24T10:00:00Z",
      "last_message_at": "2026-07-24T11:30:00Z",
      "message_count": 42,
      "workdir": "e:/projects/foo",
      "title": "解释入口逻辑",          // 首条用户消息摘要
      "provider": "anthropic",
      "model": "claude-sonnet-4",
      "tokens_total": 12345,
      "last_compaction_id": "msg_01H..."  // 最近的压缩摘要消息 id；null/缺省=未压缩，从文件头读
    }
  ]
}
```

索引在每次 append 后异步更新（写失败不影响主流程，下次启动重建）。`last_compaction_id` 在压缩产生摘要行时同步更新；`--resume` 时读此字段定位起始行，避免全文件扫描找 `parent_uuid = null`。索引损坏时回退到尾向扫描（O(N)，仅异常路径）。

### 3.2 写入策略

- **追加写**：`OpenOptions::append(true).create(true)`，每条记录 `write_all` 后 `flush`。
- **批量**：单轮内连续 append 不每次 fsync，仅 `flush`；轮结束时 `sync_all` 一次。
- **崩溃安全**：JSONL 每行完整，崩溃最多丢最后一行（未 flush 部分）；启动时校验最后一行 JSON 合法性，非法则截断。

### 3.3 读取与回放

`Storage::load` 默认**线性逐行解析**，不建 DAG。`parent_uuid` 仅作为字段透传给上层（`ContextManager` 在需要 Fork/Side-chain 检视时才用）。普通 `--resume` 路径下，线性顺序即等价于 parent 链顺序：

```rust
impl Storage for JsonlStorage {
    async fn load(&self, session: &SessionId) -> Result<Vec<Message>> {
        let path = self.dir.join(format!("{session}.jsonl"));
        // 读 index.json 取 last_compaction_id（若无则从头），定位起始字节偏移
        let start_offset = self.lookup_compaction_offset(session).await?;
        let mut reader = BufReader::new(File::open(&path)?);
        reader.seek(SeekFrom::Start(start_offset))?;
        let mut messages = Vec::new();
        let mut line = String::new();
        let mut prev_id: Option<String> = None;   // 用于回填旧文件缺失的 parent_uuid
        while reader.read_line(&mut line)? > 0 {
            let record: SessionRecord = serde_json::from_str(&line)
                .map_err(|e| StorageError::Corrupt { line: line.clone(), source: e })?;
            match record {
                SessionRecord::Message(mut m) => {
                    // 前向兼容：旧文件无 parent_uuid，按线性顺序回填
                    if m.parent_uuid.is_none() && !m.is_compaction_summary() {
                        m.parent_uuid = prev_id.clone();
                    }
                    prev_id = Some(m.id.clone());
                    messages.push(m);
                }
                SessionRecord::Compression(_) => { /* 重建压缩状态 */ }
                _ => {}
            }
            line.clear();
        }
        Ok(messages)
    }
}
```

`lookup_compaction_offset` 先查 `index.json` 的 `last_compaction_id`，找到对应行的字节偏移（首行带偏移缓存）；索引缺失时回退到尾向扫描找最近的 `parent_uuid = null` 摘要行。Fork/Side-chain 检视等稀有路径走单独的 `load_as_dag` 方法（按 `parent_uuid` 组装），不在默认热路径上。

---

## 4. 上下文快照与压缩日志

### 4.1 `CompressionStep`

```rust
pub struct CompressionStep {
    pub kind: CompressKind,
    pub affected: Vec<String>,    // 被影响的 message id
    pub tokens_saved: usize,
    pub detail: serde_json::Value,
}

pub enum CompressKind {
    ToolResultTruncate,    // tool_result 被截断
    ToolResultSummarize,   // 旧 tool_result 替换为占位
    Summarize,             // 多条消息被摘要替换
    Drop,                  // 直接丢弃
    HardTruncate,          // 兜底截断
}
```

### 4.2 快照用途

- **回放**：`--replay` 时按 compression 日志重建压缩后状态，而非重新压缩。
- **调试**：用户可查看"为什么这条消息没了"。
- **回滚**：理论上可撤销压缩（保留原始摘要前的副本到单独文件）。

### 4.3 压缩前备份

配置 `compression.keep_backup = true` 时，被替换的原始消息写入 `{session_id}.backup.jsonl`，仅供调试，不影响主流程。

---

## 5. 权限决策存储 `policy.toml`

```toml
# 由"always allow/deny"交互生成
[[allow]]
tool = "fs.write"
[allow.match]
glob = "src/**"

[[allow]]
tool = "shell.run"
[allow.match]
command_prefix = ["cargo ", "git status", "git diff"]

[[deny]]
tool = "shell.run"
[deny.match]
command_prefix = ["rm -rf", "sudo", "dd "]

[[deny]]
tool = "fs.write"
[deny.match]
glob = "{.git,.env,*.secret}/**"
```

### 5.1 匹配规则

| 字段 | 适用工具 | 语义 |
|------|---------|------|
| `glob` | fs.* | 相对 workdir 的 glob，支持 `**` |
| `command_prefix` | shell.run | 命令前缀匹配（trim 后） |
| `domain` | web.* | 域名 glob，如 `*.github.com` |
| `path_prefix` | fs.* | 路径前缀（绝对或相对 workdir） |

### 5.2 优先级（两层模型）

`policy.toml` 的 `allow`/`deny` 条目作为 L1 用户策略的 specificity=2 条目（见 `design.md` §9.5、`security.md` §2.3），与 granular rules（specificity=3~5）、`ApprovalMode`（specificity=1）、per-tool 默认矩阵（specificity=0）在同一命名空间按 specificity 降序竞争：

```
L0  内置安全黑名单 (危险命令/SSRF/敏感路径)   ← 最高，不可覆盖
L1  用户策略（按 specificity 降序匹配）
      specificity 5  granular 精确路径
      specificity 4  granular 通配路径
      specificity 3  granular 工具类别 / MCP server / 命令前缀
      specificity 2  policy.toml 显式 allow/deny（本节）
      specificity 1  ApprovalMode × SideEffect 全局平移
      specificity 0  per-tool 默认矩阵（兜底）
    最高 specificity 命中生效；同 specificity → deny 胜出
```

同 specificity 按声明顺序，首条匹配生效。`policy.toml` 内部 `deny` 与 `allow` 同 specificity 时 `deny` 胜出（safe default）。

---

## 6. 记忆存储

### 6.1 长期记忆（双文件：`long_term.md` + `long_term.index.json`）

格式规范、frontmatter 元信息、索引结构、mtime 缓存与写入原子性详见 **`design.md` §8.2/§8.3**（权威定义）。此处仅给出存储层视角：

- `long_term.md`：人机共读 Markdown，按 `## <key>` 分节，每节带 `source | updated | confidence` 元信息头。
- `long_term.index.json`：程序化查询索引，与正文同源同步，原子更新（写临时文件 → rename）。结构：`{ v, entries: [{key, topic, tags, line, tokens, updated}], total_tokens }`。
- `MemoryStore` 启动时校验索引与正文一致；不一致则以正文为准重建索引，并打 warn。

```markdown
# Long-term Memory

## pref.lang
source: user | updated: 2026-07-24 | confidence: 0.9
通信语言：中文

## conv.tab_indent
source: user | updated: 2026-07-24 | confidence: 1.0
本项目使用 tab 缩进
```

### 6.2 会话摘要 `sessions/{id}.md`

```markdown
# Session sess_01H...
- 时间：2026-07-24 10:00–11:30
- 工作目录：e:/projects/foo
- 摘要：重构了 utils 模块，拆分为 path.rs / output.rs；新增 3 个测试。
- 关键文件：src/utils/path.rs, src/utils/output.rs
- 待办：output.rs 的截断逻辑未覆盖大文件测试
```

### 6.3 索引（后续）

阶段 3 引入轻量向量索引（基于 `candle` 或调用 embedding API），支持 `@memory` 语义检索。

### 6.4 项目记忆指令层（AGENTS.md，参考 Codex/CC）

与 §6.1 的"动态长期记忆"互补，`AGENTS.md` 是**静态指令层**：用户手写、随仓库版本化、Agent 不可自主编辑。完整加载算法、override 语义、fallback 文件名、安全约束见 `design.md` §8.6（权威定义），本节仅给存储层视角。

**文件位置**：

| 层 | 路径 | 说明 |
|----|------|------|
| 全局 | `$MINICODING_HOME/AGENTS.md` | 跨项目通用约定（如"始终用中文回复"） |
| 全局 override | `$MINICODING_HOME/AGENTS.override.md` | 优先于全局 AGENTS.md（取首个非空） |
| 项目（每级目录） | `<dir>/AGENTS.md` | 从 repo_root 到 cwd 逐级，每级至多取一个 |
| 项目 override | `<dir>/AGENTS.override.md` | 优先于同目录 AGENTS.md |
| fallback | `<dir>/CLAUDE.md`、`<dir>/.cursorrules` 等 | 跨工具兼容，配置 `project.project_doc_fallback_filenames` |

**与 `long_term.md` 的区别**：

| 维度 | `long_term.md` | `AGENTS.md` |
|------|----------------|-------------|
| 维护方 | 用户 + Agent（隐式摘要） | 仅用户（Agent 不可自主编辑） |
| 作用域 | 跨项目（用户全局） | 仓库内（随版本控制） |
| 性质 | 动态记忆（偏好、决策） | 静态指令（约定、规范、禁区） |
| 存储 | `$MINICODING_HOME/memory/` | `$MINICODING_HOME/`（全局）+ 仓库各目录（项目） |
| 加载时机 | 每会话首轨注入 | 每会话首轨注入（Explore/Plan 子 Agent 跳过） |
| token 预算 | ≤10% 上下文（§3.4） | 32 KiB 截断（`project_doc_max_bytes`） |

**加载结果**：`ProjectDocLoader::load`（`api.md` §3.10）返回拼接后的字符串，注入到 `system` 消息段，包裹 `<project_doc>` 边界。截断发生在累计字节超 `project_doc_max_bytes`（默认 32768）时，静默截断并打 `tracing::warn!`。

**安全**：`fs.write`/`fs.edit` 对任意层级的 `AGENTS.md` / `AGENTS.override.md` / fallback 文件默认 `Verdict::Ask`，且 LLM 不得通过任何工具绕过该确认（参考 Codex 约束）。这与 `rules.md` C-04 凭证保护思路一致：关键配置文件的修改必须显式确认。

**MCP 配置存储**（关联，见 `design.md` §19.4）：

| 文件 | 位置 | 作用 |
|------|------|------|
| `mcp.json` | `$MINICODING_HOME/mcp.json` | local + user 作用域 MCP server 配置 |
| `mcp.json` | `<repo_root>/.minicoding/mcp.json` | project 作用域（入版本控制，团队共享） |
| `mcp_choices.toml` | `$MINICODING_HOME/mcp_choices.toml` | project 作用域 server 的逐人批准记忆 |

`mcp_choices.toml` 结构：

```toml
# 记录用户对哪些 project 作用域 MCP server 批准/拒绝
[[choices]]
repo_root = "e:/projects/foo"
server = "github"
decision = "allow"            # allow | deny
chosen_at = "2026-07-24T10:00:00Z"
```

首次遇到含 `.minicoding/mcp.json` 的仓库时，逐个 server 弹窗询问，结果写入此文件；`minicoding mcp reset-project-choices` 清空。

---

## 7. 凭证存储

| 来源 | 优先级 | 说明 |
|------|--------|------|
| 环境变量（`ANTHROPIC_API_KEY`） | 1 | CI/容器场景首选 |
| OS keyring（`keyring` crate） | 2 | 交互场景首选，`minicoding auth login` 写入 |
| 配置文件 `api_key` 字段 | 3 | **强烈不推荐**，仅本地调试；启动告警 |

`auth` 子命令：

```
minicoding auth login --provider anthropic
  > 输入密钥（不回显）→ 写入 keyring

minicoding auth status
minicoding auth logout --provider anthropic
```

---

## 8. 日志文件

`logs/minicoding.YYYY-MM-DD.log`，`tracing-appender` 滚动：

```
2026-07-24T10:00:00.123Z INFO session=sess_01H... turn=1 turn started
2026-07-24T10:00:00.456Z DEBUG session=sess_01H... llm provider=anthropic model=claude-sonnet-4 request_tokens=512
2026-07-24T10:00:01.789Z INFO session=sess_01H... tool name=fs.read elapsed_ms=3 bytes=1024
```

- 默认 `INFO`，`-v` 开 `DEBUG`，`-vv` 开 `TRACE`。
- 文件始终 `DEBUG`（受 `RUST_LOG` 覆盖）。
- 保留 7 天，超出自动清理。

---

## 9. 数据生命周期

| 数据 | 保留策略 |
|------|---------|
| 会话 jsonl | 永久（用户手动 `session prune --before` 清理） |
| 压缩备份 | 默认不保留；开启时 30 天 |
| 会话摘要 | 永久 |
| 日志文件 | 7 天滚动 |
| 临时文件（工具中间产物） | 会话结束即删 |

---

## 10. 一致性与并发

- **单会话单写者**：同一 session 同一时刻只有一个 Runtime 持有写句柄。
- **跨进程互斥**：会话目录下 `.lock` 文件（`fs2` 锁），启动时尝试获取，失败则提示会话被占用。
- **读多写少**：`index.json` 读取无锁，写时原子替换（写临时文件 → rename）。
- **跨平台路径**：使用 `camino::Utf8PathBuf`，避免 Windows 非 UTF-8 路径边界问题。

---

## 11. 性能预算

| 操作 | 目标 | 实现 |
|------|------|------|
| 单条消息 append | < 1ms | 缓冲写 + 批量 flush |
| 加载 1000 条消息会话 | < 200ms | 流式解析，零拷贝 |
| index.json 列出 1000 会话 | < 50ms | 纯元数据，无 IO 读 jsonl |
| 长期记忆注入 | < 5ms | 文件 mtime 缓存，未变直接复用 |

---

## 12. 备份与导出

- `minicoding session export <id> --format md`：导出为 Markdown 对话记录。
- `minicoding session export <id> --format jsonl`：原始格式复制。
- `minicoding backup create`：打包 `~/.minicoding/` 为 tar.gz。
- 配置 `backup.auto = "weekly"` 时自动备份（后续）。
