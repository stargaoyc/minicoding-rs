#!/usr/bin/env python3
"""从 ~/.minicoding/sessions 会话文件提取评估指标。

每个任务执行前记录 sessions 目录快照，执行后找到新增/更新的会话文件，
解析其 JSONL：
- steps（步数）= assistant 消息数（每轮 LLM 调用）+ tool_calls 总数
- output_tokens = assistant metadata.tokens 之和
- tool_calls = 工具调用总数
- cost = output_tokens * 单价（按模型从单价表估算；未配置单价时输出 0）
"""
import json
import os
from pathlib import Path

SESSIONS_DIR = Path(os.environ.get("MINICODING_HOME", Path.home() / ".minicoding")) / "sessions"

# 每 M output token 单价（美元）。评估默认模型；可经 MINICODING_EVAL_PRICE 覆盖。
PRICE_PER_M = {
    "agnes-2.5-flash": 0.30,
    "gpt-4o": 2.50,
    "gpt-4o-mini": 0.60,
    "deepseek-chat": 0.28,
    "claude-sonnet-4": 3.00,
}


def session_snapshot() -> dict[str, tuple[float, int]]:
    """返回 {session_id: (mtime, size)} 快照，用于识别新增/更新会话。"""
    snap = {}
    if not SESSIONS_DIR.exists():
        return snap
    for f in SESSIONS_DIR.glob("*.jsonl"):
        if f.name.endswith(".events.jsonl") or f.name == "index.json":
            continue
        st = f.stat()
        snap[f.name] = (st.st_mtime, st.st_size)
    return snap


def extract_metrics(before: dict[str, tuple[float, int]]) -> dict:
    """对比 before 快照，提取新增/更新会话的指标。"""
    after = session_snapshot()
    metrics = {"steps": 0, "tool_calls": 0, "output_tokens": 0, "cost_usd": 0.0, "session": ""}
    for name, (mt, size) in after.items():
        b = before.get(name)
        # 新增或 mtime 更新（size 增大）
        if b is None or b[0] < mt:
            metrics["session"] = name
            _parse_session(SESSIONS_DIR / name, metrics)
            break
    return metrics


def _parse_session(path: Path, metrics: dict) -> None:
    steps = 0
    tool_calls = 0
    out_tokens = 0
    for line in path.read_text(errors="ignore").splitlines():
        try:
            m = json.loads(line)
        except json.JSONDecodeError:
            continue
        if m.get("role") == "assistant":
            steps += 1
            tool_calls += len(m.get("tool_calls") or [])
            out_tokens += (m.get("metadata") or {}).get("tokens") or 0
    metrics["steps"] = steps
    metrics["tool_calls"] = tool_calls
    metrics["output_tokens"] = out_tokens


def cost_for(model: str, output_tokens: int) -> float:
    price = float(os.environ.get("MINICODING_EVAL_PRICE", PRICE_PER_M.get(model, 0.0)))
    return round(output_tokens / 1_000_000 * price, 5)
