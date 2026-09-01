/**
 * 可观测性 trace 存储（R10 前端全流程可观测）。
 *
 * 记录每个 SSE 事件的摘要（类型、时间、载荷摘要），供 TracePanel 展示
 * agent loop 的完整运行流程（step 边界、工具调用、思考过程、权限请求等）。
 * Token 事件高频节流：只记录首尾 + 计数。
 */
import { create } from "zustand";
import type { EventDto } from "../api/client";

/** 单条事件日志。 */
export interface TraceEntry {
  /** 单调递增序号（同 SSE seq）。 */
  seq: number;
  /** 事件类型（简短名）。 */
  type: string;
  /** 事件发生时间（毫秒）。 */
  ts: number;
  /** 人类可读摘要。 */
  summary: string;
  /** 原始载荷（JSON 缩略，可展开查看全文）。 */
  detail: string;
}

interface TraceState {
  /** 当前会话的事件日志（按 seq 升序）。 */
  entries: TraceEntry[];
  /** 面板是否打开。 */
  open: boolean;
  /** 添加到日志（自动节流高频事件）。 */
  push: (event: EventDto) => void;
  /** 切换面板。 */
  toggle: () => void;
  /** 清空日志（切换会话时调用）。 */
  clear: () => void;
  /** 打开面板。 */
  setOpen: (v: boolean) => void;
}

/** 高频事件节流计数器（token/reasoning_delta 等）。 */
let tokenCount = 0;
/** 本 turn 累积的思考文本（reasoning_delta 逐 token 到达，合并为一段）。 */
let reasoningAccum = "";
/** 最近一次 reasoning 条目的 seq（用于替换而非追加）。 */
let reasoningSeq = -1;

export const useTraceStore = create<TraceState>((set, get) => ({
  entries: [],
  open: false,
  push: (event: EventDto) => {
    const { entries } = get();
    const ts = Date.now();
    const type = event.type;
    let summary = "";
    let detail = "";

    // 高频事件（token/reasoning_delta）节流：只记录首尾 + 计数
    if (type === "token") {
      tokenCount++;
      if (tokenCount === 1) {
        summary = `💬 token 开始: "${event.text.slice(0, 40)}${event.text.length > 40 ? "…" : ""}"`;
        detail = event.text;
      } else {
        // 面板关闭时每 100 个 token 更新一次累计；打开时每次更新
        if (tokenCount % 100 !== 0 && !get().open) return;
        const next = [...entries];
        if (next.length > 0 && next[next.length - 1].type === "token") {
          next[next.length - 1] = {
            seq: event.seq,
            type: "token",
            ts,
            summary: `💬 token 流: ${tokenCount} 个 token`,
            detail: `${tokenCount} 个 token，最后片段: "${event.text.slice(0, 40)}${event.text.length > 40 ? "…" : ""}"`,
          };
          set({ entries: next });
          return;
        }
        summary = `💬 token 流: ${tokenCount} 个 token`;
        detail = `${tokenCount} 个 token`;
      }
    } else if (type === "reasoning_delta") {
      // R10 可读性：reasoning_delta 逐 token 到达，合并为一段完整思考（避免
      // 一个字母一条日志）。累积到 reasoningAccum，替换最近一条 reasoning 条目。
      // R10 性能：面板关闭时不 set（token 节流同款）——推理时每秒几十个 delta，
      // 每次 set 都触发 TracePanel 重渲染（即便不可见），是流式卡顿的来源之一。
      reasoningAccum += event.text;
      if (!get().open) return;
      const summary = `💭 思考: "${reasoningAccum.slice(0, 80)}${reasoningAccum.length > 80 ? "…" : ""}"`;
      const detail = reasoningAccum;
      const next = [...entries];
      if (reasoningSeq >= 0 && next.some((e) => e.seq === reasoningSeq)) {
        const idx = next.findIndex((e) => e.seq === reasoningSeq);
        next[idx] = { seq: event.seq, type: "reasoning_delta", ts, summary, detail };
        reasoningSeq = event.seq;
      } else {
        next.push({ seq: event.seq, type: "reasoning_delta", ts, summary, detail });
        reasoningSeq = event.seq;
      }
      set({ entries: next });
      return;
    } else if (type === "message_appended") {
      summary = `📝 消息已落盘: ${event.message.role}[${event.message.id.slice(-8)}]`;
      detail = JSON.stringify(event.message, null, 2);
      tokenCount = 0; // 新 turn 重置 token 计数
      reasoningAccum = ""; // 新 turn 重置思考累积
      reasoningSeq = -1;
    } else if (type === "turn_streaming_started") {
      summary = "▶️ turn 开始";
      detail = "";
      tokenCount = 0;
      reasoningAccum = "";
      reasoningSeq = -1;
    } else if (type === "turn_end") {
      summary = `⏹️ turn 结束: ${typeof event.stop_reason === "string" ? event.stop_reason : JSON.stringify(event.stop_reason)}`;
      detail = typeof event.stop_reason === "string" ? event.stop_reason : JSON.stringify(event.stop_reason);
    } else if (type === "tool_call_started") {
      const input = event.input && typeof event.input === "object"
        ? JSON.stringify(event.input).slice(0, 80)
        : "";
      summary = `🔧 工具调用开始: ${event.tool}`;
      detail = `call_id: ${event.call_id}\ninput: ${input}${input.length >= 80 ? "…" : ""}`;
    } else if (type === "tool_call_finished") {
      const ok = event.result.is_error ? "❌" : "✅";
      summary = `${ok} 工具调用完成: ${event.result.content.type}`;
      detail = JSON.stringify(event.result, null, 2).slice(0, 200);
    } else if (type === "permission_requested") {
      summary = `🔐 权限请求: ${event.tool} (${event.risk})`;
      detail = event.summary;
    } else if (type === "permission_resolved") {
      const decision = typeof event.decision === "string" ? event.decision : JSON.stringify(event.decision);
      summary = `🔓 权限已决: ${decision}`;
      detail = JSON.stringify(event.decision);
    } else if (type === "step_started") {
      summary = `📋 step 开始 (#${event.iter})`;
      detail = `tool_calls: ${event.tool_call_ids.join(", ")}`;
    } else if (type === "step_ended") {
      summary = `✅ step 结束 (#${event.iter})`;
      detail = "";
    } else if (type === "permission_mode_changed") {
      summary = `🔄 权限模式: ${event.from} → ${event.to}`;
      detail = "";
    } else if (type === "task_updated") {
      summary = `📌 任务更新: ${event.task.status}`;
      detail = JSON.stringify(event.task, null, 2).slice(0, 200);
    } else if (type === "config_changed") {
      summary = "⚙️ 配置变更";
      detail = "";
    } else if (type === "session_created") {
      summary = `🆕 会话创建: ${event.id.slice(-8)}`;
      detail = event.id;
    } else {
      summary = `❓ ${type}`;
      detail = JSON.stringify(event);
    }

    set({ entries: [...entries, { seq: event.seq, type, ts, summary, detail }] });
  },
  toggle: () => set((s) => ({ open: !s.open })),
  clear: () => {
    tokenCount = 0;
    reasoningAccum = "";
    reasoningSeq = -1;
    set({ entries: [] });
  },
  setOpen: (v) => set({ open: v }),
}));
