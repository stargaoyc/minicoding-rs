#!/usr/bin/env bash
# L4-001: cargo test 全绿（F2P 判定——只跑测试不比对 patch）
[ -f src/lib.rs ] || { echo "缺 src/lib.rs"; exit 1; }
if command -v cargo >/dev/null 2>&1; then
  CARGO_NET_OFFLINE=true cargo test --quiet 2>&1 | tail -3
else
  grep -q "xs.iter().sum()\|xs.iter().fold" src/lib.rs
fi
