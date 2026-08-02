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
          <p className="text-sm leading-snug">{truncate(task.content, 200)}</p>
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
