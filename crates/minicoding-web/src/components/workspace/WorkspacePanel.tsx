import { useState, useEffect } from "react";
import { motion } from "framer-motion";
import {
  ChevronRight,
  ChevronDown,
  File,
  Folder,
  FolderOpen,
  RefreshCw,
  FolderGit2,
  FileDiff,
  ArrowRight,
} from "lucide-react";
import { Button } from "../ui/button";
import { ScrollArea } from "../ui/scroll-area";
import {
  useWorkspaceRoot,
  useWorkspaceList,
  useWorkspaceDiff,
  useSwitchWorkspace,
} from "../../hooks/useWorkspace";
import { useUIStore } from "../../stores/ui";
import { cn, displayPath } from "../../lib/utils";
import type { WorkspaceListEntry, WorkspaceFileChange } from "../../api/generated";

/**
 * 项目工作区面板（W-11，见 design.md §26.9）。
 *
 * 布局：侧栏下方折叠区。展示当前 workdir 根路径 + 懒加载文件树；
 * 点击文件在底部预览面板显示内容（只读，≤ 64 KiB）；"改动"按钮弹出
 * diff 面板（会话内 journal 记录）；"切换"按钮弹出目标路径输入框
 * （Ask 审批弹窗由 SSE `permission_requested` 复用 W-03 机制弹出）。
 *
 * 安全边界（与后端契约）：浏览只读不弹权限（等价 `fs.read`，C-01 仅约束
 * 副作用）；越界路径后端返回 403（C-03），前端 toast 展示不吞错。
 */
export function WorkspacePanel({ sessionId }: { sessionId: string | null }) {
  const [open, setOpen] = useState(false);
  const { data: root } = useWorkspaceRoot(sessionId);

  if (!sessionId) return null;

  return (
    <div className="border-t border-[var(--color-border)]">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-4 py-2.5 text-left text-xs font-medium text-[var(--color-text-muted)] hover:bg-[var(--color-surface-2)] transition-colors"
      >
        {open ? <ChevronDown className="h-3.5 w-3.5" /> : <ChevronRight className="h-3.5 w-3.5" />}
        <FolderGit2 className="h-3.5 w-3.5" />
        项目工作区
        {root && (
          <span className="truncate text-[10px] text-[var(--color-text-muted)]">{root.name}</span>
        )}
      </button>

      {open && (
        <motion.div
          initial={{ height: 0, opacity: 0 }}
          animate={{ height: "auto", opacity: 1 }}
          exit={{ height: 0, opacity: 0 }}
          className="overflow-hidden"
        >
          <WorkspaceBody sessionId={sessionId} />
        </motion.div>
      )}
    </div>
  );
}

/** 工作区主体：根路径条 + 文件树 + 操作按钮。 */
function WorkspaceBody({ sessionId }: { sessionId: string }) {
  const { data: root, isLoading: rootLoading } = useWorkspaceRoot(sessionId);
  const switchWs = useSwitchWorkspace(sessionId);
  const [target, setTarget] = useState("");
  const [switching, setSwitching] = useState(false);
  const [switchError, setSwitchError] = useState<string | null>(null);
  const [diffOpen, setDiffOpen] = useState(false);

  const handleSwitch = async () => {
    const path = target.trim();
    if (!path) return;
    setSwitchError(null);
    setSwitching(true);
    try {
      const resp = await switchWs.mutateAsync(path);
      if (!resp.switched) {
        setSwitchError("用户拒绝了切换请求");
      } else {
        setTarget("");
      }
    } catch (e) {
      setSwitchError(e instanceof Error ? e.message : String(e));
    } finally {
      setSwitching(false);
    }
  };
  return (
    <div className="flex flex-col gap-1 px-3 pb-3">
      {rootLoading ? (
        <p className="px-1 py-2 text-xs text-[var(--color-text-muted)]">加载中…</p>
      ) : root ? (
        <div className="flex items-center gap-1 px-1">
          <span
            className="flex-1 truncate font-mono text-[10px] text-[var(--color-text-muted)]"
            title={displayPath(root.path)}
          >
            {displayPath(root.path)}
          </span>
          <Button
            variant="ghost"
            size="icon"
            className="h-5 w-5"
            title="打开文件改动（diff）"
            onClick={() => setDiffOpen(true)}
          >
            <FileDiff className="h-3 w-3" />
          </Button>
        </div>
      ) : null}

      {/* 切换工作区 */}
      <div className="flex items-center gap-1">
        <input
          value={target}
          onChange={(e) => setTarget(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && void handleSwitch()}
          placeholder="输入绝对路径切换工作区…"
          className="min-w-0 flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] px-2 py-1 text-[10px] text-[var(--color-text)] outline-none focus:border-[var(--color-accent)]"
        />
        <Button
          variant="secondary"
          size="sm"
          className="h-6 px-2 text-[10px]"
          disabled={switching || !target.trim()}
          onClick={() => void handleSwitch()}
        >
          {switching ? (
            <RefreshCw className="h-3 w-3 animate-spin" />
          ) : (
            <ArrowRight className="h-3 w-3" />
          )}
          切换
        </Button>
      </div>
      {switching && (
        <p className="px-1 text-[10px] text-[var(--color-text-muted)]">
          等待授权确认…（权限弹窗未出现时 65s 后超时）
        </p>
      )}
      {switchError && (
        <p className="px-1 text-[10px] text-[var(--color-risk-high)]">{switchError}</p>
      )}

      {/* 文件树 */}
      <ScrollArea className="max-h-56">
        <FileTree sessionId={sessionId} path="" depth={0} />
      </ScrollArea>

      <DiffDialog sessionId={sessionId} open={diffOpen} onClose={() => setDiffOpen(false)} />
    </div>
  );
}

/**
 * 递归文件树（懒加载：目录首次展开时拉取子列表，缓存于 TanStack Query）。
 */
function FileTree({
  sessionId,
  path,
  depth,
}: {
  sessionId: string;
  /** 相对路径（根目录为 ""）。 */
  path: string;
  depth: number;
}) {
  const { data, isLoading, isError } = useWorkspaceList(sessionId, path);
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const [previewPath, setPreviewPath] = useState<string | null>(null);
  const setPreview = useUIStore((s) => s.setPreview);

  // 打开预览时同步到全局 store（App 底部面板渲染）
  useEffect(() => {
    if (previewPath) setPreview(previewPath);
  }, [previewPath, setPreview]);

  if (isLoading)
    return <p className="py-1 pl-2 text-[10px] text-[var(--color-text-muted)]">加载中…</p>;
  if (isError)
    return <p className="py-1 pl-2 text-[10px] text-[var(--color-risk-high)]">加载失败</p>;
  if (!data || data.entries.length === 0) {
    return <p className="py-1 pl-2 text-[10px] text-[var(--color-text-muted)]">（空目录）</p>;
  }

  return (
    <div className={cn(depth > 0 && "border-l border-[var(--color-border)] pl-2")}>
      {data.entries.map((entry) => (
        <TreeNode
          key={entry.name}
          sessionId={sessionId}
          entry={entry}
          parentPath={path}
          depth={depth}
          expanded={expanded}
          onToggle={(name) => setExpanded((e) => ({ ...e, [name]: !e[name] }))}
          onOpenFile={(rel) => {
            setPreviewPath(rel);
          }}
        />
      ))}
    </div>
  );
}

function TreeNode({
  sessionId,
  entry,
  parentPath,
  depth,
  expanded,
  onToggle,
  onOpenFile,
}: {
  sessionId: string;
  entry: WorkspaceListEntry;
  parentPath: string;
  depth: number;
  expanded: Record<string, boolean>;
  onToggle: (name: string) => void;
  onOpenFile: (rel: string) => void;
}) {
  const isDir = entry.kind === "dir";
  const rel = parentPath ? `${parentPath}/${entry.name}` : entry.name;
  const isExpanded = !!expanded[entry.name];

  return (
    <div>
      <button
        onClick={() => (isDir ? onToggle(entry.name) : onOpenFile(rel))}
        className="flex w-full items-center gap-1 rounded px-1 py-0.5 text-left text-[11px] text-[var(--color-text)] hover:bg-[var(--color-surface-2)] transition-colors"
      >
        {isDir ? (
          <>
            {isExpanded ? (
              <ChevronDown className="h-3 w-3 shrink-0 text-[var(--color-text-muted)]" />
            ) : (
              <ChevronRight className="h-3 w-3 shrink-0 text-[var(--color-text-muted)]" />
            )}
            {isExpanded ? (
              <FolderOpen className="h-3 w-3 shrink-0 text-[var(--color-accent)]" />
            ) : (
              <Folder className="h-3 w-3 shrink-0 text-[var(--color-accent)]" />
            )}
          </>
        ) : (
          <>
            <span className="w-3 shrink-0" />
            <File className="h-3 w-3 shrink-0 text-[var(--color-text-muted)]" />
          </>
        )}
        <span className="truncate">{entry.name}</span>
        {!isDir && entry.size != null && (
          <span className="ml-auto text-[9px] text-[var(--color-text-muted)]">
            {formatSize(entry.size)}
          </span>
        )}
      </button>
      {isDir && isExpanded && <FileTree sessionId={sessionId} path={rel} depth={depth + 1} />}
    </div>
  );
}

/** diff 面板：会话内文件改动历史（journal，`GET /workspace/diff`）。 */
function DiffDialog({
  sessionId,
  open,
  onClose,
}: {
  sessionId: string;
  open: boolean;
  onClose: () => void;
}) {
  const { data, isLoading } = useWorkspaceDiff(sessionId);
  const [selected, setSelected] = useState<{
    before: string | null;
    after: string | null;
    name: string;
  } | null>(null);

  useEffect(() => {
    if (!open) setSelected(null);
  }, [open]);

  if (!open) return null;

  const changes = data?.entries.flatMap((e) => e.files) ?? [];

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
      onClick={(e) => e.target === e.currentTarget && onClose()}
    >
      <motion.div
        initial={{ scale: 0.95, opacity: 0, y: 10 }}
        animate={{ scale: 1, opacity: 1, y: 0 }}
        className="flex h-[70vh] w-[80vw] max-w-3xl flex-col overflow-hidden rounded-xl border border-[var(--color-border)] bg-[var(--color-surface)] shadow-2xl"
      >
        <div className="flex items-center justify-between border-b border-[var(--color-border)] px-4 py-2.5">
          <span className="text-sm font-medium">文件改动（会话内）</span>
          <button
            onClick={onClose}
            className="text-xs text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
          >
            关闭
          </button>
        </div>

        {isLoading ? (
          <div className="flex flex-1 items-center justify-center text-sm text-[var(--color-text-muted)]">
            加载中…
          </div>
        ) : changes.length === 0 ? (
          <div className="flex flex-1 items-center justify-center text-sm text-[var(--color-text-muted)]">
            本会话暂无文件改动
          </div>
        ) : (
          <div className="flex min-h-0 flex-1">
            {/* 改动列表 */}
            <div className="w-64 shrink-0 overflow-y-auto border-r border-[var(--color-border)]">
              {changes.map((c, i) => (
                <button
                  key={i}
                  onClick={() => setSelected(changeToPair(c))}
                  className="flex w-full items-center gap-2 px-3 py-2 text-left text-xs hover:bg-[var(--color-surface-2)]"
                >
                  <ChangeBadge kind={c.kind} />
                  <span className="truncate">{fileShortName(c)}</span>
                </button>
              ))}
            </div>
            {/* 内容对比 */}
            <div className="min-w-0 flex-1 overflow-auto p-4">
              {selected ? (
                <pre className="whitespace-pre-wrap font-mono text-[11px] leading-relaxed">
                  {selected.before != null && (
                    <>
                      <span className="text-[var(--color-risk-high)]">{selected.before}</span>
                      <span className="my-1 block border-t border-[var(--color-border)]" />
                    </>
                  )}
                  <span className="text-[var(--color-risk-low)]">{selected.after}</span>
                </pre>
              ) : (
                <p className="text-xs text-[var(--color-text-muted)]">
                  点击左侧改动查看内容对比（红=改动前，绿=改动后）
                </p>
              )}
            </div>
          </div>
        )}
      </motion.div>
    </div>
  );
}

function ChangeBadge({ kind }: { kind: string }) {
  const map: Record<string, string> = {
    created: "bg-[var(--color-risk-low)]/15 text-[var(--color-risk-low)]",
    written: "bg-[var(--color-accent)]/15 text-[var(--color-accent-hover)]",
    edited: "bg-[var(--color-accent)]/15 text-[var(--color-accent-hover)]",
    deleted: "bg-[var(--color-risk-high)]/15 text-[var(--color-risk-high)]",
  };
  const label: Record<string, string> = {
    created: "新建",
    written: "写入",
    edited: "编辑",
    deleted: "删除",
  };
  return (
    <span
      className={cn(
        "rounded px-1 py-0.5 text-[9px] font-medium",
        map[kind] ?? "bg-[var(--color-surface-2)] text-[var(--color-text-muted)]",
      )}
    >
      {label[kind] ?? kind}
    </span>
  );
}

function fileShortName(c: WorkspaceFileChange): string {
  // 显示层清理：Windows `\\?\` 前缀 + 反斜杠 → 取最后一段
  const path = displayPath(c.path);
  return path.split("/").pop() ?? path;
}

function changeToPair(c: WorkspaceFileChange): {
  before: string | null;
  after: string | null;
  name: string;
} {
  switch (c.kind) {
    case "created":
      return { before: null, after: c.content, name: c.path };
    case "written":
      return { before: c.before, after: c.after, name: c.path };
    case "edited":
      return { before: c.before, after: c.after, name: c.path };
    case "deleted":
      return { before: c.content, after: null, name: c.path };
  }
}

function formatSize(n: bigint): string {
  const num = Number(n);
  if (num < 1024) return `${num} B`;
  if (num < 1024 * 1024) return `${(num / 1024).toFixed(1)} KB`;
  return `${(num / (1024 * 1024)).toFixed(1)} MB`;
}
