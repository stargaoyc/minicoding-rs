# M9 设计文档 — Web 与桌面应用

> **注记**：本文为 M9 设计快照，架构/技术栈以 `docs/design.md` §26 与 `docs/tech-stack.md` 为准，冲突时以后者为准。
>
> **里程碑**：M9（可选，低优先级）
> **预估**：12 人日 / 8 task
> **依赖**：M8（`minicoding-server` HTTP/SSE JSON-RPC 稳定）
> **状态**：已实现（W-01..W-10 全部交付 + Q-08/Q-09 分发配置）

---

## 目录

- [1. 目标与范围](#1-目标与范围)
- [2. 部署架构](#2-部署架构)
- [3. 技术栈选型](#3-技术栈选型)
- [4. 项目结构](#4-项目结构)
- [5. 前端核心设计](#5-前端核心设计)
- [6. Tauri 桌面集成](#6-tauri-桌面集成)
- [7. 安全模型](#7-安全模型)
- [8. 构建工具链](#8-构建工具链)
- [9. 性能目标](#9-性能目标)
- [10. 测试策略](#10-测试策略)
- [11. 任务分解](#11-任务分解)
- [12. 风险与缓解](#12-风险与缓解)

---

## 1. 目标与范围

### 1.1 目标

为 `minicoding-rs` 提供 **Web 前端** 与 **原生桌面应用**，降低非终端用户的上手门槛。核心原则：**Rust 后端不嵌入前端**，前端通过 HTTP/SSE JSON-RPC 与 `minicoding-server` 通信，保证 CLI/SDK 可独立使用。

### 1.2 交付物

| 交付物 | 说明 |
|--------|------|
| `crates/minicoding-web/` | 纯前端项目（React 19.2 + TS 7.0 + Vite 8.1），独立 `package.json` |
| `crates/minicoding-desktop/` | Tauri 2.x 桌面壳，加入 Cargo workspace |
| `minicoding serve --web` | 静态资源托管（单二进制部署） |
| `--cors-origin` | CORS 配置（仅 Web 模式需要） |

### 1.3 非目标

- Tauri 2.x mobile（留待 M10+）
- 前端自定义工具编辑器
- 多用户协作（单用户模型）

---

## 2. 部署架构

### 2.1 两种部署形态

```
┌─────────────────────────────────────────────────────────────┐
│  形态 A：Web 模式（浏览器）                                    │
│                                                              │
│  ┌──────────────────┐    HTTPS    ┌───────────────────────┐ │
│  │  Browser          │ ──────────► │  minicoding-server    │ │
│  │  (React 19.2 SPA) │ ◄────────── │  --bind 127.0.0.1:8080 --web ./dist  │ │
│  │                   │    SSE      │  --cors-origin ...    │ │
│  └──────────────────┘             └───────────┬───────────┘ │
│                                                │             │
│                                     ┌──────────▼──────────┐  │
│                                     │  Runtime (Agent)    │  │
│                                     └─────────────────────┘  │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│  形态 B：桌面模式（Tauri 2.x）                                 │
│                                                              │
│  ┌──────────────────────┐  Tauri IPC  ┌──────────────────┐  │
│  │  Tauri Window         │ ──────────► │  Rust sidecar    │  │
│  │  (WebView + dist/)    │ ◄────────── │  minicoding-server│  │
│  │                       │    SSE      │  --bind 127.0.0.1 │  │
│  └──────────────────────┘             └────────┬─────────┘  │
│                                                │            │
│  OS 集成：托盘 / 快捷键 / 自动更新              │            │
│  凭证：OS keyring（C-04）                      │            │
│                                     ┌──────────▼──────────┐  │
│                                     │  Runtime (Agent)    │  │
│                                     └─────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

### 2.2 数据流

```
用户输入 ──► POST /api/rpc {method: "session.send", params: {msg}}
                    │
                    ▼
            Runtime Agent 循环
                    │
            ┌───────┼───────┐
            ▼       ▼       ▼
         Token  ToolCall  PermissionRequest
            │       │       │
            └───────┼───────┘
                    ▼
            SSE EventStream
                    │
                    ▼
            前端增量渲染
```

---

## 3. 技术栈选型

### 3.1 前端栈

| 层 | 选型 | 版本 | 选型理由 |
|----|------|------|---------|
| 框架 | React | 19.2 | React Compiler 稳定，自动 memo 化 |
| 语言 | TypeScript | 7.0 | 原生 ESM，性能提升 |
| 构建 | Vite (Rolldown) | 8.1 | Rust 写的 bundler，构建速度 10x |
| 路由 | TanStack Router | 1.170 | 类型安全路由，无运行时开销 |
| 数据 | TanStack Query | 5.101 | SSE 增量缓存更新 |
| 状态 | Zustand | 5.0 | 轻量全局状态（权限弹窗/主题） |
| UI | shadcn/ui | latest | 复制粘贴源码，无运行时依赖 |
| 样式 | Tailwind CSS | v4 (Oxide) | Rust 写的引擎，CSS-first 配置 |
| 校验 | Zod | 4.4 | JSON-RPC 响应运行时校验 |
| Lint | oxlint | latest | Rust 写的 linter，50x 快 |
| Format | oxfmt | latest | Rust 写的 formatter |

### 3.2 桌面栈

| 层 | 选型 | 版本 | 选型理由 |
|----|------|------|---------|
| 桌面壳 | Tauri | 2.x | Rust 实现，体积 5-10MB（Electron 100MB+） |
| IPC | Tauri Commands | 2.x | Rust 命令直接调用，无序列化开销 |
| 更新 | Tauri Updater | 2.x | 签名校验自动更新 |
| 托盘 | Tauri System Tray | 2.x | 原生系统托盘集成 |

### 3.3 为何选 Tauri 而非 Electron

| 维度 | Tauri 2.x | Electron |
|------|-----------|----------|
| 体积 | 5-10 MB | 100 MB+ |
| 内存 | 30-50 MB | 100-200 MB |
| 安全 | Rust 内存安全 + CSP | Node.js 难以管控 |
| IPC | Rust 命令直接调用 | JSON 序列化 |
| Mobile | 2.x 支持 | 不支持 |

Tauri 与本项目"Rust 一等公民"理念一致。

---

## 4. 项目结构

### 4.1 Workspace 集成

```
minicoding-rs/                   # Cargo workspace 根
├── crates/
│   ├── ...                      # M0-M8 现有 crate
│   ├── minicoding-desktop/      # M9 新增：Tauri 壳（Cargo crate）
│   │   ├── Cargo.toml
│   │   ├── tauri.conf.json
│   │   ├── src/
│   │   │   ├── main.rs          # Tauri 入口 + sidecar 管理
│   │   │   ├── tray.rs          # 系统托盘
│   │   │   ├── shortcut.rs      # 全局快捷键
│   │   │   └── updater.rs       # 自动更新
│   │   └── icons/
│   └── minicoding-web/          # M9 新增：纯前端（独立 package.json）
│       ├── package.json         # 不属于 Cargo workspace
│       ├── vite.config.ts
│       ├── tailwind.config.ts
│       ├── oxlint.config.json
│       ├── tsconfig.json
│       ├── index.html
│       └── src/
│           ├── main.tsx         # React 19.2 入口
│           ├── router.tsx       # TanStack Router
│           ├── routes/          # 文件路由
│           ├── api/             # JSON-RPC + SSE 客户端
│           ├── hooks/           # React hooks
│           ├── components/      # UI 组件
│           ├── store/           # Zustand stores
│           └── lib/             # 工具函数
```

### 4.2 前端模块职责

```
src/
├── api/
│   ├── client.ts        # JSON-RPC 2.0 fetch 封装 + Zod 校验
│   ├── sse.ts           # EventSource 订阅 + cursor 断线重连
│   └── schema.ts        # 由 minicoding-protocol DTO 生成的 Zod schema
├── hooks/
│   ├── useSession.ts    # 会话消息 useQuery 封装
│   ├── useEventStream.ts # SSE → queryClient.setQueryData 增量更新
│   └── usePermission.ts # 权限弹窗状态（Zustand）
├── components/
│   ├── ui/              # shadcn/ui（Button/Dialog/Input/...）
│   ├── chat/            # 对话流（消息列表 + 流式 token）
│   ├── tools/           # 工具调用展开/折叠面板
│   ├── permission/      # 权限确认 Dialog
│   ├── tasks/           # 任务进度面板
│   └── theme/           # 暗色/亮色切换
├── store/
│   ├── sessionStore.ts  # 当前会话 / 面板开关
│   └── themeStore.ts    # 主题偏好
└── lib/
    ├── rpc.ts           # JSON-RPC 类型推导
    └── utils.ts         # cn() 等工具
```

---

## 5. 前端核心设计

### 5.1 流式 Token 渲染

SSE 推送 `Event::Token`，前端用 TanStack Query 增量更新消息缓存：

```typescript
// hooks/useEventStream.ts
function useEventStream(sessionId: string, cursor: number) {
  const queryClient = useQueryClient();
  const es = new EventSource(`/api/sessions/${sessionId}/events?cursor=${cursor}`);

  es.addEventListener('Token', (e) => {
    const token = TokenEventSchema.parse(JSON.parse(e.data));
    queryClient.setQueryData(['session', sessionId, 'messages'], (old = []) => {
      const last = old[old.length - 1];
      if (last?.role === 'assistant' && last.streaming) {
        return [...old.slice(0, -1), { ...last, content: last.content + token.delta }];
      }
      return [...old, { role: 'assistant', content: token.delta, streaming: true }];
    });
  });

  // ToolCall / ToolResult / PermissionRequest / TaskUpdated 同理
  return () => es.close();
}
```

**设计要点**：
- `cursor` 用于断线重连（SSE E-13），前端存储最后收到的 event_id
- React Compiler 自动 memo 化消息列表，无需手写 `useMemo`/`React.memo`
- 流式渲染用 CSS `contain: layout style` 隔离重绘范围

### 5.2 权限确认弹窗

权限交互是**点对点**（非广播），前端通过 SSE 收到 `PermissionRequest` 后弹出 Dialog：

```
1. Runtime: PermissionPolicy::check → Verdict::Ask(prompt)
2. Server:  生成 prompt_id (ULID)，SSE 推送 PermissionRequest{prompt_id, ...}
3. 前端:    usePermissionStore 收到 → shadcn/ui Dialog 弹出
4. 用户:    点击 Allow / Deny / AllowAlways
5. 前端:    POST /api/rpc { method: "permission.resolve", params: {prompt_id, decision} }
6. Server:  校验 prompt_id → Prompter::resolve → 返回 Decision
7. Runtime: 收到 Decision，继续执行或拒绝
```

```typescript
// components/permission/PermissionDialog.tsx
function PermissionDialog() {
  const { prompt, resolve } = usePermissionStore();
  if (!prompt) return null;

  return (
    <Dialog open onOpenChange={() => resolve('deny')}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{prompt.tool_name}</DialogTitle>
          <DialogDescription>{prompt.summary}</DialogDescription>
        </DialogHeader>
        <RiskBadge level={prompt.risk} />
        <div className="flex gap-2 justify-end">
          <Button variant="outline" onClick={() => resolve('deny')}>拒绝</Button>
          <Button variant="outline" onClick={() => resolve('allow_always')}>始终允许</Button>
          <Button onClick={() => resolve('allow')}>允许</Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
```

**安全**：`prompt_id` 由后端生成（ULID），前端不可伪造；`permission.resolve` 必须带有效 `prompt_id`。

### 5.3 多会话面板

```
┌─────────────┬──────────────────────────────────┐
│ 会话列表     │ 对话流                             │
│             │                                   │
│ ▸ Session A │  User: 帮我创建一个 React 组件      │
│   Session B │  Assistant: 我来帮你...             │
│   Session C │    ┌─ fs.read package.json ✓       │
│             │    └─ fs.write Component.tsx ✓     │
│ + 新建       │                                   │
│             │  [输入框]                    [发送] │
└─────────────┴──────────────────────────────────┘
```

- 左侧：会话列表（TanStack Query `useQuery(['sessions'])`）
- 右侧：对话流 + 工具面板 + 输入框
- 路由：`/sessions/$sessionId`（TanStack Router 类型安全）

### 5.4 工具调用面板

```typescript
// components/tools/ToolCallPanel.tsx
function ToolCallPanel({ call, result }: { call: ToolCall; result?: ToolResult }) {
  const [expanded, setExpanded] = useState(false);
  return (
    <Collapsible open={expanded} onOpenChange={setExpanded}>
      <CollapsibleTrigger className="flex items-center gap-2">
        <StatusIcon status={result?.status} />
        <code>{call.tool}</code>
        <ChevronIcon />
      </CollapsibleTrigger>
      <CollapsibleContent>
        <pre className="bg-muted p-2 rounded text-xs">
          {JSON.stringify(call.input, null, 2)}
        </pre>
        {result && (
          <pre className="bg-muted p-2 rounded text-xs mt-1">
            {result.output}
          </pre>
        )}
      </CollapsibleContent>
    </Collapsible>
  );
}
```

### 5.5 主题切换

```typescript
// store/themeStore.ts
import { create } from 'zustand';

type Theme = 'light' | 'dark' | 'system';
interface ThemeStore {
  theme: Theme;
  setTheme: (t: Theme) => void;
}

export const useThemeStore = create<ThemeStore>((set) => ({
  theme: (localStorage.getItem('theme') as Theme) ?? 'system',
  setTheme: (theme) => {
    localStorage.setItem('theme', theme);
    document.documentElement.classList.toggle('dark', theme === 'dark');
    set({ theme });
  },
}));
```

Tailwind v4 CSS-first 配置，`dark:` 前缀自动响应 `.dark` class。

---

## 6. Tauri 桌面集成

### 6.1 Sidecar 启动

桌面模式下，Tauri 启动 `minicoding-server` 作为 sidecar：

```rust
// crates/minicoding-desktop/src/main.rs
use tauri::Manager;

#[tauri::command]
async fn start_sidecar(app: tauri::AppHandle) -> Result<u16, String> {
    let sidecar = app.shell()
        .sidecar("minicoding-server")
        .map_err(|e| e.to_string())?;

    let (mut rx, _child) = sidecar
        .args(["--bind", "127.0.0.1:0"])
        .spawn()
        .map_err(|e| e.to_string())?;

    // 读取 sidecar stdout 获取实际监听端口
    while let Some(event) = rx.recv().await {
        if let tauri_plugin_shell::process::CommandEvent::Stdout(line) = event {
            if let Some(port) = line.trim().strip_prefix("LISTENING_PORT=") {
                return Ok(port.parse().unwrap_or(0));
            }
        }
    }
    Err("sidecar 启动失败".to_string())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .invoke_handler(tauri::generate_handler![start_sidecar])
        .setup(|app| {
            // 系统托盘
            tray::setup(app)?;
            // 全局快捷键
            shortcut::setup(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

### 6.2 系统托盘

```rust
// crates/minicoding-desktop/src/tray.rs
use tauri::{SystemTray, SystemTrayMenu, SystemTrayMenuItem, CustomMenuItem};

pub fn setup(app: &tauri::App) -> tauri::Result<()> {
    let quit = CustomMenuItem::new("quit", "退出");
    let show = CustomMenuItem::new("show", "显示窗口");
    let tray_menu = SystemTrayMenu::new()
        .add_item(show)
        .add_native_item(SystemTrayMenuItem::Separator)
        .add_item(quit);

    SystemTray::new()
        .with_menu(tray_menu)
        .on_event(|event| {
            match event.event() {
                tauri::SystemTrayEvent::DoubleClick { .. } => {
                    // 双击托盘图标显示窗口
                }
                tauri::SystemTrayEvent::MenuItemClick { id, .. } => {
                    match id.as_str() {
                        "quit" => std::process::exit(0),
                        "show" => { /* show window */ }
                        _ => {}
                    }
                }
                _ => {}
            }
        })
        .build(app)?;
    Ok(())
}
```

### 6.3 全局快捷键

```rust
// crates/minicoding-desktop/src/shortcut.rs
use tauri::{GlobalShortcut, Manager};

pub fn setup(app: &tauri::App) -> tauri::Result<()> {
    let shortcut = app.global_shortcut();
    shortcut.on_shortcut("Ctrl+Alt+M", move |app, _event| {
        if let Some(window) = app.get_window("main") {
            if window.is_visible().unwrap_or(false) {
                let _ = window.hide();
            } else {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
    })?;
    Ok(())
}
```

### 6.4 自动更新

```json
// tauri.conf.json
{
  "updater": {
    "active": true,
    "endpoints": [
      "https://releases.minicoding.dev/{{target}}/{{arch}}/{{current_version}}"
    ],
    "pubkey": "公钥（签名校验）"
  }
}
```

```rust
// crates/minicoding-desktop/src/updater.rs
use tauri_updater::Updater;

pub async fn check_update(app: &tauri::AppHandle) -> Result<(), String> {
    let updater = Updater::new(app)
        .map_err(|e| e.to_string())?;
    if let Some(update) = updater.check().await.map_err(|e| e.to_string())? {
        update.download_and_install().await.map_err(|e| e.to_string())?;
        app.restart();
    }
    Ok(())
}
```

### 6.5 凭证存储

桌面端复用 OS keyring（与 CLI `cred.rs` 共享 `KEYRING_SERVICE = "minicoding"`），前端不接触凭证明文：

```
前端需要 API key → invoke('get_credential_status') → {has_key: true, masked: "sk-***"}
前端设置 API key → invoke('set_credential', {key: "sk-..."}) → OS keyring 存储
```

---

## 7. 安全模型

### 7.1 威胁矩阵

| 威胁 | 缓解 | 约束 ID |
|------|------|---------|
| XSS 注入（工具输出含恶意脚本） | React 默认转义 + CSP `script-src 'self'` + 不用 `dangerouslySetInnerHTML`（除非经 DOMPurify） | C-05 |
| 权限弹窗伪造 | `prompt_id` 后端生成（ULID），`permission.resolve` 校验有效性 | C-01 |
| 凭证泄露到前端 | 凭证仅存 Rust 后端 + OS keyring，前端只看 `***` 脱敏 | C-04 |
| SSE 跨会话串流 | SSE 端点校验 `session_id` 归属当前认证用户 | — |
| Tauri WebView 远程内容 | Tauri 默认禁用远程内容，仅加载本地 `dist/` | — |
| CORS 误配 | `--cors-origin` 默认仅 `http://localhost:*` | — |

### 7.2 CSP 策略

```
Content-Security-Policy:
  default-src 'self';
  script-src 'self';
  style-src 'self' 'unsafe-inline';
  connect-src 'self' http://127.0.0.1:* ws://127.0.0.1:*;
  img-src 'self' data:;
  font-src 'self';
  object-src 'none';
  base-uri 'self';
```

### 7.3 凭证隔离

```
┌─────────────┐     invoke      ┌──────────────────┐
│  前端 (JS)   │ ──────────────► │  Tauri Rust      │
│  只见 ***    │ ◄────────────── │  OS keyring 读写  │
└─────────────┘   masked result  └──────────────────┘
                                        │
                                        ▼
                              ┌──────────────────┐
                              │  OS Keyring       │
                              │  (macOS/Win/Linux)│
                              └──────────────────┘
```

前端永远不接触 API key 明文，所有凭证操作经 Tauri Rust 命令代理。

---

## 8. 构建工具链

### 8.1 全 Rust 工具链

| 工具 | 用途 | 语言 | 速度优势 |
|------|------|------|---------|
| Vite (Rolldown) | JS/TS bundler | Rust | 10x vs webpack |
| Tailwind v4 (Oxide) | CSS engine | Rust | 5x vs v3 |
| oxlint | JS/TS linter | Rust | 50x vs ESLint |
| oxfmt | JS/TS formatter | Rust | 20x vs Prettier |
| tsc 7.0 | 类型检查 | TypeScript | 原生 ESM |

### 8.2 构建命令

```bash
# 前端开发
cd crates/minicoding-web
pnpm install
pnpm dev          # Vite dev server (HMR)

# 前端构建
pnpm build        # → dist/ (静态资源)

# 桌面开发
cd crates/minicoding-desktop
cargo tauri dev   # Tauri + Vite dev

# 桌面打包
cargo tauri build # → .dmg / .msi / .AppImage

# 静态资源托管（单二进制部署）
minicoding serve --bind 127.0.0.1:8080 --web ./crates/minicoding-web/dist
```

### 8.3 CI 集成

```yaml
# .github/workflows/ci.yml 新增 M9 job
web:
  name: Web lint + build
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v4
    - uses: pnpm/action-setup@v4
    - run: cd crates/minicoding-web && pnpm install --frozen-lockfile
    - run: cd crates/minicoding-web && pnpm lint    # oxlint
    - run: cd crates/minicoding-web && pnpm build   # tsc + Vite

desktop:
  name: Tauri build (${{ matrix.os }})
  strategy:
    matrix:
      os: [ubuntu-latest, macos-latest, windows-latest]
  runs-on: ${{ matrix.os }}
  steps:
    - uses: actions/checkout@v4
    - uses: dtolnay/rust-toolchain@nightly
    - run: cd crates/minicoding-desktop && cargo tauri build
```

---

## 9. 性能目标

| 指标 | 目标 | 测量方式 |
|------|------|---------|
| Lighthouse Performance | ≥ 90 | Chrome DevTools |
| Lighthouse Accessibility | ≥ 95 | Chrome DevTools |
| 首屏加载 (FCP) | < 1s | Vite preview + Lighthouse |
| SSE → 渲染延迟 | < 50ms | Performance API |
| 桌面应用体积 | < 15 MB | `cargo tauri build` 产物 |
| 桌面内存占用 | < 80 MB | 活动监视器 |
| 1000 消息滚动 | 60fps | Chrome Performance |

---

## 10. 测试策略

### 10.1 前端测试

| 层 | 工具 | 覆盖 |
|----|------|------|
| 单元 | Vitest | hooks / store / utils |
| 组件 | Testing Library + Vitest | Dialog / Chat / Tools |
| E2E | Playwright | 完整对话流 + 权限确认 |
| 视觉 | Storybook + Chromatic | UI 回归 |

### 10.2 Mock 策略

```typescript
// tests/mocks/server.ts
import { setupServer } from 'msw/node';
import { http, HttpResponse } from 'msw';

export const mockServer = setupServer(
  // JSON-RPC mock
  http.post('/api/rpc', async ({ request }) => {
    const body = await request.json();
    if (body.method === 'session.list') {
      return HttpResponse.json({ result: [{ id: 'test', summary: 'Test' }] });
    }
    return HttpResponse.json({ error: { code: -32601, message: 'not found' } });
  }),
  // SSE mock
  http.get('/api/sessions/:id/events', () => {
    return new HttpResponse(streamSSEEvents(), { headers: { 'Content-Type': 'text/event-stream' } });
  }),
);
```

### 10.3 桌面测试

- Tauri sidecar 启动/端口解析：Rust 单元测试
- 系统托盘 / 快捷键：手动测试（OS 依赖）
- 自动更新：mock endpoint 单元测试

---

## 11. 任务分解

| Task | 描述 | 依赖 |
|------|------|------|
| T-M9-1 | `minicoding-web` 项目初始化：package.json + Vite + Tailwind v4 + oxlint + TS 7.0 | M8 完成 |
| T-M9-2 | JSON-RPC 客户端 + SSE 订阅 + Zod schema 生成（从 `minicoding-protocol` DTO） | T-M9-1 |
| T-M9-3 | 核心组件：对话流（流式 token）+ 工具面板 + 权限 Dialog | T-M9-2 |
| T-M9-4 | 多会话面板 + TanStack Router 路由 + 主题切换 | T-M9-3 |
| T-M9-5 | `minicoding serve --web` 静态托管 + `--cors-origin` 配置 | T-M9-2 |
| T-M9-6 | `minicoding-desktop` Tauri 壳：sidecar + 托盘 + 快捷键 | T-M9-4 |
| T-M9-7 | 自动更新 + OS keyring 凭证集成 | T-M9-6 |
| T-M9-8 | CI 集成（web lint/build + desktop 三平台 build）+ 文档 | T-M9-7 |

---

## 12. 风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| React Compiler 仍 RC | 中 | 中 | 评估稳定性，必要时回退手写 `useMemo`/`React.memo` |
| Tauri 2.x mobile 不稳定 | 低 | 低 | M9 仅桌面，mobile 留待 M10+ |
| 前端 XSS 攻击 | 低 | 高 | CSP 严格 + React 默认转义 + DOMPurify 兜底 |
| SSE 断线重连丢事件 | 中 | 中 | cursor 恢复（E-13）+ 前端重连后请求缺失区间 |
| 桌面体积超标 | 低 | 低 | Tauri 默认 5-10MB，远低于 15MB 上限 |
| oxlint/oxfmt 不稳定 | 低 | 低 | 回退 ESLint/Prettier（性能差但功能完整） |

---

## 附录 A：与现有文档的关系

| 文档 | 章节 | 关系 |
|------|------|------|
| `docs/design.md` | §26 | 本文档是 §26 的展开实现细节 |
| `docs/tech-stack.md` | §4.1 | 技术栈选型来源 |
| `docs/features.md` | §12.5 (W-01..W-10) | 功能项对应 |
| `docs/roadmap.md` | M9 | 里程碑范围与验收标准 |
| `docs/design.md` | §24 | JSON-RPC + SSE 协议定义（M9 前端消费，不引入新协议） |

## 附录 B：验收清单

- [ ] `minicoding serve --bind 127.0.0.1:8080 --web ./dist` 启动后，浏览器能完整对话/工具调用/权限确认
- [ ] Tauri 桌面应用在 macOS/Windows/Linux 三平台可构建，体积 < 15MB
- [ ] 前端 Lighthouse Performance ≥ 90
- [ ] oxlint + tsc 全绿
- [ ] 凭证经 OS keyring 存储，不出现在前端代码/日志/网络请求中
- [ ] SSE 断线重连后 cursor 恢复，不丢事件
- [ ] 暗色/亮色主题切换正确
- [ ] 多会话面板可切换、可新建、可删除
