import { useState } from "react";
import { motion } from "framer-motion";
import { FolderOpen, Loader2, ShieldAlert, ShieldCheck } from "lucide-react";
import { Button } from "../ui/button";
import { isTauri, selectWorkspaceDir } from "../../api/tauri";
import { cn } from "../../lib/utils";

/** 会话安全模式（新建会话时选定，映射到 `permission_mode`/`preset`/`plan_mode`）。 */
export type SessionModeKey = "default" | "accept_edits" | "plan" | "full_access";

const MODE_OPTIONS: {
  key: SessionModeKey;
  label: string;
  desc: string;
  danger?: boolean;
}[] = [
  {
    key: "default",
    label: "默认",
    desc: "文件编辑与命令执行均需确认（最安全）",
  },
  {
    key: "accept_edits",
    label: "编辑自动",
    desc: "工作区内文件编辑自动执行，命令仍确认（推荐）",
  },
  {
    key: "plan",
    label: "先规划",
    desc: "先写 plan.md 拆分任务，批准后执行（适合大任务）",
  },
  {
    key: "full_access",
    label: "全自动·沙箱外",
    desc: "不弹窗且绕过沙箱——仅限受信隔离容器内使用",
    danger: true,
  },
];

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
  onConfirm: (workdir: string | undefined, mode: SessionModeKey) => void;
  onClose: () => void;
}) {
  const [workdir, setWorkdir] = useState("");
  const [picking, setPicking] = useState(false);
  const [pickError, setPickError] = useState<string | null>(null);
  const [mode, setMode] = useState<SessionModeKey>("accept_edits");
  const [ackDanger, setAckDanger] = useState(false);

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
    onConfirm(trimmed ? trimmed : undefined, mode);
  };

  const dangerMode = mode === "full_access";

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

        <div className="flex flex-col gap-1">
          <label className="text-xs text-[var(--color-text-muted)]">权限模式</label>
          <div className="grid grid-cols-2 gap-1.5">
            {MODE_OPTIONS.map((opt) => (
              <button
                key={opt.key}
                type="button"
                onClick={() => setMode(opt.key)}
                className={cn(
                  "flex flex-col gap-0.5 rounded-md border px-2 py-1.5 text-left transition-all",
                  mode === opt.key
                    ? opt.danger
                      ? "border-[var(--color-risk-high)]/60 bg-[var(--color-risk-high)]/10"
                      : "border-[var(--color-accent)]/60 bg-[var(--color-accent)]/10"
                    : "border-[var(--color-border)] bg-[var(--color-surface)] hover:bg-[var(--color-surface-2)]",
                )}
              >
                <span
                  className={cn(
                    "flex items-center gap-1 text-[11px] font-medium",
                    opt.danger
                      ? "text-[var(--color-risk-high)]"
                      : "text-[var(--color-text)]",
                  )}
                >
                  {opt.danger ? (
                    <ShieldAlert className="h-3 w-3" />
                  ) : (
                    <ShieldCheck className="h-3 w-3" />
                  )}
                  {opt.label}
                </span>
                <span className="text-[10px] leading-tight text-[var(--color-text-muted)]">
                  {opt.desc}
                </span>
              </button>
            ))}
          </div>
          {dangerMode && (
            <div className="flex flex-col gap-1 rounded-md border border-[var(--color-risk-high)]/40 bg-[var(--color-risk-high)]/5 px-2.5 py-2">
              <p className="text-[11px] font-medium text-[var(--color-risk-high)]">
                ⚠ 沙箱外全自动运行：AI 可直接修改文件、执行命令、访问网络，不弹任何确认。
              </p>
              <p className="text-[10px] text-[var(--color-text-muted)]">
                请仅在受信任的隔离容器（如专用 VM/Docker）内启用（C-22）。桌面应用请使用"编辑自动"模式。
              </p>
              <label className="flex items-center gap-1.5 text-[11px] text-[var(--color-text)]">
                <input
                  type="checkbox"
                  checked={ackDanger}
                  onChange={(e) => setAckDanger(e.target.checked)}
                  className="h-3.5 w-3.5 accent-[var(--color-risk-high)]"
                />
                我已了解风险，仍要启用
              </label>
            </div>
          )}
        </div>

        <div className="flex justify-end gap-2">
          <Button variant="ghost" size="sm" disabled={creating} onClick={onClose}>
            取消
          </Button>
          <Button size="sm" disabled={creating || (dangerMode && !ackDanger)} onClick={handleConfirm}>
            {creating ? <Loader2 className="h-3 w-3 animate-spin" /> : null}
            {dangerMode ? "启用全自动并创建" : "创建"}
          </Button>
        </div>
      </motion.div>
    </div>
  );
}
