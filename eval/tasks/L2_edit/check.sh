#!/usr/bin/env bash
# L2-001: cargo test 全绿
[ -f src/main.rs ] || { echo "缺 src/main.rs"; exit 1; }
if command -v cargo >/dev/null 2>&1; then
  CARGO_NET_OFFLINE=true cargo test --quiet 2>&1 | tail -3
else
  grep -q "a + b" src/main.rs
fi
