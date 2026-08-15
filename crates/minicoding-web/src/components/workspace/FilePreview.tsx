import { useCallback } from "react";
import { motion } from "framer-motion";
import { ExternalLink, Trash2 } from "lucide-react";
import { useWorkspaceFile } from "../../hooks/useWorkspace";
import { useUIStore } from "../../stores/ui";
import { isTauri, openWorkspaceFile } from "../../api/tauri";
import { Button } from "../ui/button";

/**
 * 文件内容预览面板（W-11，底部滑出）。
 *
 * 只读视图（≤ 64 KiB，超出截断标记，C-07），内容来自后端
 * `GET /sessions/{id}/workspace/read`（C-03 路径沙箱在后端强制）。
 * 桌面端提供"用系统编辑器打开"（Tauri `open_workspace_file` 命令，
 * 走系统默认编辑器，不经前端权限链路）。
 */
export function FilePreview({ sessionId }: { sessionId: string | null }) {
  const { previewPath, previewOpen, setPreview } = useUIStore();
  const { data, isLoading, isError } = useWorkspaceFile(sessionId, previewPath);

  const handleOpenExternal = useCallback(() => {
    if (!isTauri() || !previewPath) return;
    void openWorkspaceFile(previewPath as string).catch((e: unknown) => {
      console.warn("open workspace file failed", e);
    });
  }, [previewPath]);

  if (!sessionId || !previewOpen || !previewPath) return null;

  return (
    <motion.div
      initial={{ y: "100%" }}
      animate={{ y: 0 }}
      exit={{ y: "100%" }}
      transition={{ type: "spring", stiffness: 300, damping: 32 }}
      className="flex h-72 flex-col border-t border-[var(--color-border)] bg-[var(--color-surface)]"
    >
      {/* 头部：文件名 + 操作 */}
      <div className="flex items-center gap-2 border-b border-[var(--color-border)] px-4 py-2">
        <span className="flex-1 truncate font-mono text-xs text-[var(--color-text)]">
          {previewPath}
        </span>
        {data && (
          <span className="text-[10px] text-[var(--color-text-muted)]">
            {data.size} B{data.truncated ? " · 已截断（≤ 64 KiB）" : ""}
          </span>
        )}
        {isTauri() && (
          <Button
            variant="ghost"
            size="sm"
            className="h-6 px-2 text-[10px]"
            onClick={handleOpenExternal}
          >
            <ExternalLink className="h-3 w-3" />
            系统编辑器打开
          </Button>
        )}
        <Button
          variant="ghost"
          size="icon"
          className="h-6 w-6"
          onClick={() => setPreview(null)}
          title="关闭预览"
        >
          <Trash2 className="h-3 w-3" />
        </Button>
      </div>

      {/* 内容 */}
      <div className="min-h-0 flex-1 overflow-auto p-4">
        {isLoading ? (
          <p className="text-xs text-[var(--color-text-muted)]">加载中…</p>
        ) : isError ? (
          <p className="text-xs text-[var(--color-risk-high)]">文件读取失败（可能越界或不存在）</p>
        ) : data ? (
          <pre className="whitespace-pre-wrap font-mono text-[11px] leading-relaxed text-[var(--color-text)]">
            {data.content}
          </pre>
        ) : null}
      </div>
    </motion.div>
  );
}
