#!/usr/bin/env python3
"""
MiniCode 四层评估 runner（调研见 interview/coding-agent-benchmarks.md）。

逐任务：
1. 在隔离临时目录（workspace）中准备任务环境（fixtures/setup.sh 或按需生成）
2. 调 `minicoding exec`（真实或 mock LLM）执行 prompt
3. 运行判定脚本（check.sh）判定 pass/fail
4. 汇总 resolution rate / cost / 失败模式

用法：
  python3 eval/runner.py                       # mock LLM（框架验证）
  OPENAI_API_KEY=sk-xxx python3 eval/runner.py --real   # 真实 LLM
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
TASKS_DIR = ROOT / "tasks"
MINICODING = ROOT.parent / "target" / "debug" / "minicoding"
# eval 是 Python 内置函数，不能直接 from eval import → 加到 sys.path 按包名导入
sys.path.insert(0, str(ROOT.parent))
import eval.metrics as eval_metrics  # type: ignore[import-untyped]


def build_minicoding() -> None:
    """确保 CLI 已构建。"""
    if not MINICODING.exists():
        subprocess.run(
            ["cargo", "build", "-p", "minicoding-cli"], cwd=ROOT.parent, check=True
        )


def load_tasks(layer: str | None) -> list[dict]:
    """扫描 tasks/ 目录加载任务定义（task.json）。"""
    tasks: list[dict] = []
    for tdir in sorted(TASKS_DIR.iterdir()):
        if not tdir.is_dir() or tdir.name.startswith("_"):
            continue
        if layer and not tdir.name.startswith(layer):
            continue
        tj = tdir / "task.json"
        if not tj.exists():
            continue
        task = json.loads(tj.read_text())
        task["_dir"] = str(tdir)
        tasks.append(task)
    return tasks


def prepare_workspace(task: dict, base: Path) -> Path:
    """准备任务工作区：复制 fixtures/（若存在）+ 应用 setup。"""
    tdir = Path(task["_dir"])
    ws = base / "workspace"
    ws.mkdir(parents=True, exist_ok=True)
    fixtures = tdir / "fixtures"
    if fixtures.exists():
        for item in fixtures.iterdir():
            dst = ws / item.name
            if item.is_dir():
                shutil.copytree(item, dst, dirs_exist_ok=True)
            else:
                shutil.copy2(item, dst)
    return ws


def run_check(task: dict, ws: Path) -> tuple[bool, str]:
    """运行任务判定脚本。返回 (passed, output)。"""
    tdir = Path(task["_dir"])
    check = tdir / "check.sh"
    if not check.exists():
        return False, "missing check.sh"
    try:
        r = subprocess.run(
            ["bash", str(check)],
            cwd=ws,
            capture_output=True,
            text=True,
            timeout=120,
        )
        return r.returncode == 0, r.stdout[-2000:] + r.stderr[-1000:]
    except subprocess.TimeoutExpired:
        return False, "check 超时"


def run_task_once(task: dict, ws: Path, extra_args: list[str], timeout_s: int) -> dict:
    """执行一次任务：跑 minicoding exec，返回执行结果。"""
    prompt = task["prompt"]
    # exec 模式默认沙箱 read-only；本评估需要写 workspace → workspace-write
    cmd = [
        str(MINICODING), "exec",
        "--sandbox", "workspace-write",
        "--auto-approve",
        "--workdir", str(ws),
        *extra_args,
        prompt,
    ]
    t0 = time.time()
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout_s)
        elapsed = time.time() - t0
        return {
            "rc": r.returncode,
            "elapsed_s": round(elapsed, 1),
            "stdout": r.stdout[-3000:],
            "stderr": r.stderr[-2000:],
        }
    except subprocess.TimeoutExpired:
        return {"rc": -1, "elapsed_s": timeout_s, "stdout": "", "stderr": "exec 超时"}


def run_tasks(tasks: list[dict], args) -> list[dict]:
    results = []
    for task in tasks:
        tid = task["id"]
        print(f"\n=== {tid} [{task.get('layer', '?')}] {task['name']} ===")
        # 每个任务独立临时目录（不污染仓库）
        with tempfile.TemporaryDirectory(prefix="minicoding-eval-") as tmp:
            ws = prepare_workspace(task, Path(tmp))
            passed, check_out = False, ""
            attempts = task.get("attempts", 1)
            task_metrics = {"steps": 0, "tool_calls": 0, "output_tokens": 0, "cost_usd": 0.0, "session": ""}
            for attempt in range(1, attempts + 1):
                print(f"  [attempt {attempt}/{attempts}]")
                before = eval_metrics.session_snapshot()
                exec_result = run_task_once(task, ws, args.extra_args, args.timeout)
                m = eval_metrics.extract_metrics(before)
                if m["steps"] > 0:
                    task_metrics = m  # 取最后一次成功执行的会话
                if exec_result["rc"] != 0:
                    print(f"  exec rc={exec_result['rc']}（错误，见输出）")
                    if args.verbose:
                        print("  stderr:", exec_result["stderr"][-800:])
                    if attempt < attempts:
                        continue
                    passed, check_out = False, exec_result["stderr"]
                    break
                # exec 成功 → 跑判定
                passed, check_out = run_check(task, ws)
                if passed:
                    print(f"  ✅ PASS（{exec_result['elapsed_s']}s）")
                    break
                print(f"  ❌ check 失败（attempt {attempt}）")
                if args.verbose:
                    print("  check:", check_out[-600:])
            results.append({
                "id": tid,
                "layer": task.get("layer"),
                "name": task["name"],
                "passed": passed,
                "elapsed_s": exec_result.get("elapsed_s", 0),
                "check_output": check_out[-800:] if not passed else "",
                "prompt": task["prompt"],
                "metrics": task_metrics,
            })
    return results


def summarize(results: list[dict]) -> None:
    layers = {}
    for r in results:
        layers.setdefault(r["layer"], []).append(r)
    print("\n\n================ 汇总 ================")
    total_pass = sum(1 for r in results if r["passed"])
    total_cost = sum(r.get("metrics", {}).get("cost_usd", 0) for r in results)
    total_tokens = sum(r.get("metrics", {}).get("output_tokens", 0) for r in results)
    total_steps = sum(r.get("metrics", {}).get("steps", 0) for r in results)
    for layer in sorted(layers):
        rs = layers[layer]
        ps = sum(1 for r in rs if r["passed"])
        layer_cost = sum(r.get("metrics", {}).get("cost_usd", 0) for r in rs)
        layer_tokens = sum(r.get("metrics", {}).get("output_tokens", 0) for r in rs)
        layer_steps = sum(r.get("metrics", {}).get("steps", 0) for r in rs)
        print(f"[{layer}] {ps}/{len(rs)} 通过 ({100*ps/max(len(rs),1):.0f}%)"
              f" | steps={layer_steps} out_tokens={layer_tokens} cost=${layer_cost:.4f}")
        for r in rs:
            mark = "PASS" if r["passed"] else "FAIL"
            m = r.get("metrics", {})
            print(f"  {mark}  {r['id']} {r['name']} ({r['elapsed_s']}s)"
                  f" | steps={m.get('steps',0)} tools={m.get('tool_calls',0)}"
                  f" out_tokens={m.get('output_tokens',0)} cost=${m.get('cost_usd',0):.5f}")
            if not r["passed"] and r["check_output"]:
                print(f"       check: {r['check_output'][:300].replace(chr(10), ' | ')}")
    print(f"\n总计: {total_pass}/{len(results)} 通过"
          f" | total_steps={total_steps} total_out_tokens={total_tokens} total_cost=${total_cost:.4f}")
    # JSON 结果输出供 CI/归档
    out = ROOT / "results.json"
    out.write_text(json.dumps(results, ensure_ascii=False, indent=2))
    print(f"结果已写入 eval/results.json")


def main():
    ap = argparse.ArgumentParser(description="MiniCode 四层评估 runner")
    ap.add_argument("--layer", default=None, help="只跑某层: L1|L2|L3|L4")
    ap.add_argument("--real", action="store_true", help="真实 LLM（需 API key）")
    ap.add_argument("--provider", default=None)
    ap.add_argument("--model", default=None)
    ap.add_argument("--api-base", default=None)
    ap.add_argument("--timeout", type=int, default=300)
    ap.add_argument("--verbose", action="store_true")
    ap.add_argument("extra_args", nargs="*")
    args = ap.parse_args()

    build_minicoding()
    tasks = load_tasks(args.layer)
    if not tasks:
        print("无任务（检查 eval/tasks/ 目录）")
        sys.exit(1)
    print(f"加载 {len(tasks)} 个任务")

    extra = []
    if args.real:
        # 真实 LLM：env 提供 key；透传 provider/model
        key = os.environ.get("OPENAI_API_KEY")
        if not key:
            print("错误：--real 需要 OPENAI_API_KEY")
            sys.exit(1)
        if args.provider:
            extra += ["--provider", args.provider]
        if args.model:
            extra += ["--model", args.model]
        if args.api_base:
            extra += ["--api-base", args.api_base]
    else:
        # mock LLM：指向本地 mock server；R10-03 fail-closed 需要显式 key
        extra += ["--api-base", os.environ.get("MINICODING_EVAL_MOCK_BASE", "http://127.0.0.1:8765")]
        extra += ["--api-key", "sk-test-eval"]
    extra += args.extra_args

    # cost 计算：需要 model 名（--real 时传的，或 env 兜底）
    model = args.model or os.environ.get("OPENAI_MODEL", "agnes-2.5-flash")
    results = run_tasks(tasks, argparse.Namespace(extra_args=extra, timeout=args.timeout, verbose=args.verbose))
    for r in results:
        m = r.get("metrics", {})
        m["cost_usd"] = eval_metrics.cost_for(model, m.get("output_tokens", 0))
        r["metrics"] = m
    summarize(results)


if __name__ == "__main__":
    main()
