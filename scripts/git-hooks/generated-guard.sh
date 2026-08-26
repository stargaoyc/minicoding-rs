#!/usr/bin/env bash
# ENG-3（2026-08-26 R3 审查）：ts-rs 导出污染防护——`cargo test --all-features`
# 会触发 ts-rs 把 DTO 写入 crates/minicoding-web/src/api/generated/（被跟踪
# 文件），测试非幂等、跑一次脏一次（已两次事故提交还原）。本守卫阻止把
# 污染产物误提交；合法重生成请走 `pnpm gen-types`（CI 有 diff 门禁兜底），
# 或临时 MINICODING_ALLOW_GEN=1 跳过本检查。
set -euo pipefail
if [ "${MINICODING_ALLOW_GEN:-0}" = "1" ]; then
  exit 0
fi
polluted=$(git diff --cached --name-only -- crates/minicoding-web/src/api/generated/ || true)
if [ -n "$polluted" ]; then
  echo "ERROR: 以下 generated 产物被修改（疑似 cargo test 的 ts-rs 导出污染）：" >&2
  echo "$polluted" >&2
  echo "如为合法 DTO 变更：请在 crates/minicoding-web 执行 pnpm gen-types 重新生成并确认 diff 一致，" >&2
  echo "或临时以 MINICODING_ALLOW_GEN=1 提交。" >&2
  exit 1
fi
