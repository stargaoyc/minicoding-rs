#!/usr/bin/env bash
# L3-001: 校验目录结构
[ -d src/backend ] && [ -d src/frontend ] || { echo "目录结构不完整"; exit 1; }
[ -f src/backend/main.rs ] && [ -f src/frontend/App.tsx ] || { echo "缺文件"; exit 1; }
echo "目录结构完整"
