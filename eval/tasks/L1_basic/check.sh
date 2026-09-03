#!/usr/bin/env bash
# L1-001: 校验 src/lib.rs 存在、含 fib 实现；有 cargo 时跑单测
[ -f src/lib.rs ] || { echo "缺 src/lib.rs"; exit 1; }
grep -q "fn fib" src/lib.rs || { echo "缺 fib 函数"; exit 1; }
if command -v cargo >/dev/null 2>&1 && [ -f Cargo.toml ]; then
  CARGO_NET_OFFLINE=true cargo test --quiet 2>&1 | tail -3
else
  echo "no cargo, skip test"
fi
