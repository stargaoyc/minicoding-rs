import { useState } from "react";
import { motion } from "framer-motion";
import { FolderOpen, Loader2 } from "lucide-react";
import { Button } from "../ui/button";
import { isTauri, selectWorkspaceDir } from "../../api/tauri";

/**
 * 新建会话对话框（W-11：先选工作目录，再新建对话）。
 *
 * - 目录输入框 +（Tauri 模式）原生目录选择器按钮
 * - 留空 = 使用默认目录（server 启动目录）
 * - 确定后经 `onConfirm(workdir?)` 回调创建会话
 *
 * 视觉风格对齐 `SetupDialog`/`PermissionDialog`（framer-motion overlay + glass panel）。
 */
export function NewSessionDialog({
  open,
  creating,
  onConfirm,
  onClose,
}: {
  open: boolean;
  /** 创建请求进行中（禁用确定按钮，防重复提交）。 */
  creating: boolean;
  onConfirm: (workdir: string | undefined) => void;
  onClose: () => void;
}) {
  const [workdir, setWorkdir] = useState("");
  const [picking, setPicking] = useState(false);
  const [pickError, setPickError] = useState<string | null>(null);

  if (!open) return null;

  const handlePick = async () => {
    setPicking(true);
    setPickError(null);
    try {
      const picked = await selectWorkspaceDir();
      if (picked) setWorkdir(picked);
    } catch (e) {
      setPickError(e instanceof Error ? e.message : String(e));
    } finally {
      setPicking(false);
    }
  };

  const handleConfirm = () => {
    const trimmed = workdir.trim();
    onConfirm(trimmed ? trimmed : undefined);
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
      onClick={(e) => e.target === e.currentTarget && !creating && onClose()}
    >
      <motion.div
        initial={{ scale: 0.95, opacity: 0, y: 10 }}
        animate={{ scale: 1, opacity: 1, y: 0 }}
        className="flex w-[480px] max-w-[90vw] flex-col gap-3 overflow-hidden rounded-xl border border-[var(--color-border)] bg-[var(--color-surface)] p-5 shadow-2xl"
      >
        <div className="flex items-center gap-2">
          <FolderOpen className="h-4 w-4 text-[var(--color-accent)]" />
          <span className="text-sm font-medium">新建会话</span>
        </div>

        <div className="flex flex-col gap-1">
          <label className="text-xs text-[var(--color-text-muted)]">
            工作目录（留空使用默认目录）
          </label>
          <div className="flex items-center gap-2">
            <input
              value={workdir}
              onChange={(e) => setWorkdir(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && !creating && handleConfirm()}
              placeholder="如 E:\projects\my-project"
              spellCheck={false}
              className="min-w-0 flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] px-2.5 py-1.5 text-xs text-[var(--color-text)] outline-none focus:border-[var(--color-accent)]"
            />
            {isTauri() && (
              <Button
                variant="secondary"
                size="sm"
                className="h-7 shrink-0 px-2 text-[10px]"
                disabled={picking || creating}
                onClick={() => void handlePick()}
                title="打开系统目录选择器"
              >
                {picking ? <Loader2 className="h-3 w-3 animate-spin" /> : <FolderOpen className="h-3 w-3" />}
                选择目录…
              </Button>
            )}
          </div>
          <p className="text-[10px] text-[var(--color-text-muted)]">
            {isTauri()
              ? "选择一个项目目录作为会话工作区，后续工具执行与文件浏览均限制在该目录内（C-03）。"
              : "留空时使用服务端启动目录。工作区内的文件操作受权限与沙箱约束（C-01/C-03）。"}
          </p>
          {pickError && <p className="text-[10px] text-[var(--color-risk-high)]">{pickError}</p>}
        </div>

        <div className="flex justify-end gap-2">
          <Button variant="ghost" size="sm" disabled={creating} onClick={onClose}>
            取消
          </Button>
          <Button size="sm" disabled={creating} onClick={handleConfirm}>
            {creating ? <Loader2 className="h-3 w-3 animate-spin" /> : null}
            创建
          </Button>
        </div>
      </motion.div>
    </div>
  );
}
