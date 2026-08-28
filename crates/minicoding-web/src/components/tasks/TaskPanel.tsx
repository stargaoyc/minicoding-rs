import { motion, AnimatePresence } from "framer-motion";
import { CheckCircle2, Circle, Clock, XCircle, ListTodo, X } from "lucide-react";
import { Button } from "../ui/button";
import { ScrollArea } from "../ui/scroll-area";
import type { Task, TaskStatus } from "../../api/generated";
import { cn, truncate } from "../../lib/utils";

const STATUS_CONFIG: Record<TaskStatus, { icon: typeof Circle; color: string; label: string }> = {
  pending: { icon: Circle, color: "text-[var(--color-text-muted)]", label: "待处理" },
  inprogress: { icon: Clock, color: "text-[var(--color-risk-medium)]", label: "进行中" },
  completed: { icon: CheckCircle2, color: "text-[var(--color-risk-low)]", label: "已完成" },
  cancelled: { icon: XCircle, color: "text-[var(--color-text-muted)]", label: "已取消" },
};

interface TaskPanelProps {
  tasks: Task[];
  open: boolean;
  onClose: () => void;
}

export function TaskPanel({ tasks, open, onClose }: TaskPanelProps) {
  return (
    <AnimatePresence>
      {open && (
        <motion.div
          initial={{ x: 320, opacity: 0 }}
          animate={{ x: 0, opacity: 1 }}
          exit={{ x: 320, opacity: 0 }}
          transition={{ type: "spring", damping: 25, stiffness: 300 }}
          className="flex w-80 flex-col border-l border-[var(--color-border)] glass"
        >
          {/* Header */}
          <div className="flex items-center justify-between px-4 py-3">
            <div className="flex items-center gap-2">
              <ListTodo className="h-4 w-4 text-[var(--color-accent-hover)]" />
              <span className="text-sm font-semibold">任务面板</span>
              <span className="text-xs text-[var(--color-text-muted)]">{tasks.length}</span>
            </div>
            <Button variant="ghost" size="icon" onClick={onClose} className="h-7 w-7">
              <X className="h-3.5 w-3.5" />
            </Button>
          </div>

          {/* Task list */}
          <ScrollArea className="flex-1 px-3 pb-3">
            {tasks.length === 0 ? (
              <div className="py-8 text-center text-sm text-[var(--color-text-muted)]">
                暂无任务
              </div>
            ) : (
              <div className="space-y-2">
                {tasks.map((task) => (
                  <TaskItem key={task.id} task={task} />
                ))}
              </div>
            )}
          </ScrollArea>
        </motion.div>
      )}
    </AnimatePresence>
  );
}

function TaskItem({ task }: { task: Task }) {
  const config = STATUS_CONFIG[task.status] ?? STATUS_CONFIG.pending;
  const Icon = config.icon;

  // ② 任务内容拆分（2026-08-28 用户反馈）：把含编号列表的任务拆成多个子项
  // 展示（如"创建应用：1. 项目结构 2. 后端 API 3. 前端…"拆成 3 行）。
  // 纯展示拆分——不改变任务 ID/状态/依赖映射（后端数据模型不变）。
  const subItems = splitTaskContent(task.content);

  return (
    <motion.div
      layout
      initial={{ opacity: 0, y: -4 }}
      animate={{ opacity: 1, y: 0 }}
      className={cn(
        "rounded-lg border border-[var(--color-border)] bg-[var(--color-surface)] p-3",
        task.status === "inprogress" && "ring-1 ring-[var(--color-risk-medium)]/30",
      )}
    >
      <div className="flex items-start gap-2">
        <Icon className={cn("mt-0.5 h-4 w-4 shrink-0", config.color)} />
        <div className="flex-1 space-y-1">
          {subItems.length > 1 ? (
            // 拆分展示：主标题（去编号前缀）+ 子项列表
            <div className="space-y-1">
              <p className="text-sm leading-snug">{truncate(subItems[0].title, 200)}</p>
              <ul className="space-y-0.5 border-l border-[var(--color-border)] pl-2">
                {subItems.map((item, i) => (
                  <li key={i} className="flex items-start gap-1.5">
                    <span className="mt-0.5 shrink-0 text-[10px] font-medium text-[var(--color-text-muted)]">
                      {item.num}.
                    </span>
                    <span className="text-xs leading-snug text-[var(--color-text)]">
                      {truncate(item.body, 180)}
                    </span>
                  </li>
                ))}
              </ul>
            </div>
          ) : (
            <p className="text-sm leading-snug">{truncate(task.content, 200)}</p>
          )}
          <div className="flex items-center gap-2">
            <span className={cn("text-[10px] font-medium", config.color)}>{config.label}</span>
            {task.summary && (
              <span className="text-[10px] text-[var(--color-text-muted)]">
                · {truncate(task.summary, 60)}
              </span>
            )}
          </div>
          {/* 依赖关系 */}
          {(task.blocks.length > 0 || task.blocked_by.length > 0) && (
            <div className="flex items-center gap-2 pt-1 text-[10px] text-[var(--color-text-muted)]">
              {task.blocks.length > 0 && <span>阻塞 {task.blocks.length} 项</span>}
              {task.blocked_by.length > 0 && <span>被 {task.blocked_by.length} 项阻塞</span>}
            </div>
          )}
        </div>
      </div>
    </motion.div>
  );
}

/**
 * 拆分任务内容中的编号列表（②，2026-08-28 用户反馈）。
 *
 * 匹配 `1. xxx` / `1、xxx` / `1) xxx` 等编号开头行（中英文编号均可）。
 * 返回 `[{ num, title, body }]`：`title` 为编号前的总起句（可能为空），
 * `body` 为各编号项内容。无编号列表时返回单元素（title=全文，body=""）。
 *
 * 纯展示逻辑：不改变 `Task` 数据模型（任务 ID/状态仍属于父任务），仅把
 * 一个任务里的编号子项拆成多行显示，满足"任务面板看到拆好的子任务"诉求。
 */
function splitTaskContent(content: string): { num: string; title: string; body: string }[] {
  const lines = content.split("\n").map((l) => l.trim()).filter((l) => l.length > 0);
  const numRe = /^(\d+)[.、)．]\s*(.*)$/;
  const items: { num: string; title: string; body: string }[] = [];
  const titleParts: string[] = [];

  for (const line of lines) {
    const m = line.match(numRe);
    if (m) {
      items.push({ num: m[1], title: "", body: m[2] });
    } else if (items.length === 0) {
      titleParts.push(line);
    } else {
      // 编号项内的续行：追加到上一个子项
      const last = items[items.length - 1];
      last.body = `${last.body} ${line}`.trim();
    }
  }

  if (items.length <= 1) {
    return [{ num: "", title: content, body: "" }];
  }
  return [{ num: "", title: titleParts.join(" "), body: "" }, ...items];
}
