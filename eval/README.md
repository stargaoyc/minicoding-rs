# MiniCode 四层评估体系

参考业界 Coding Agent 主流 Benchmark（调研见 `interview/coding-agent-benchmarks.md`），
为 MiniCode 建立从基础到端到端的四层评测金字塔。

## 评估体系

```
L4 端到端 ──────────────────────────────┐
  SWE-bench 式：issue → patch，F2P 判定   │  最贴近用户价值
                                          │
L3 终端 ────────────────────────────┐     │
  Terminal-Bench 式：shell 任务、     │     │  MiniCode 最相关的层
  最终状态判定（非 diff）              │     │
                                     │     │
L2 编辑 ────────────────────┐        │     │
  Aider polyglot 式：编辑     │        │     │  工具链核心能力
  已有代码、两次机会          │        │     │
                            │        │     │
L1 基础 ───────────┐        │        │     │
  HumanEval 式：    │        │        │     │  模型基线冒烟
  函数生成、编译判定  │        │        │     │
                   │        │        │     │
```

## 评估指标

- **Resolution Rate**（解决率）——核心指标，每层独立统计
- **Cost per Task**（每次耗时/费用）——真实 LLM 评估时统计
- **平均步数**——工具调用轮次
- **Well-formed Rate**——Agent 输出结构合规率
- **失败模式归类**——常见失败原因分布

## 使用方法

```bash
# 默认 mock LLM（验证框架可运行）
python3 eval/runner.py

# 真实 LLM（需设置 API key）
OPENAI_API_KEY=sk-xxx python3 eval/runner.py --real

# 指定 provider
OPENAI_API_KEY=sk-xxx python3 eval/runner.py --real --provider openai --model gpt-4o

# 只跑特定层
python3 eval/runner.py --layer L3
```

## 四层任务一览

| ID | 层 | 任务 | 判定方式 | 与 Minicode 能力 |
|----|----|------|---------|----------------|
| L1-001 | 基础 | Rust fib(n) 函数生成 | 编译 + 单测 | `fs.write` + `shell.run cargo test` |
| L1-002 | 基础 | 字符串反转 | 编译 + 单测 | `fs.write` + `shell.run` |
| L2-001 | 编辑 | 修复 add→sub bug | 已有仓库 + cargo test | `fs.read` + `fs.edit` + `shell.run` |
| L2-002 | 编辑 | 添加日志函数 | 已有仓库 + cargo test | `fs.read` + `fs.write` + `shell.run` |
| L3-001 | 终端 | 创建项目目录结构 | 文件存在 + 内容匹配 | `shell.run` + `fs.write` |
| L3-002 | 终端 | git 提交并推送模拟 | git log 检查 | `shell.run` 组合 |
| L3-003 | 终端 | PAT 文件查找+统计 | 命令输出匹配 | `shell.run` + `fs.read` |
| L4-001 | 端到端 | 小仓库 issue 修复 | cargo test 全绿 | 全工具链 + 多轮 |
