#!/usr/bin/env bash
# 为本地开发创建 sidecar 占位二进制（解决 tauri_build::build() 校验 externalBin 路径）。
#
# tauri.conf.json 的 externalBin 在 cargo build 时校验 binaries/minicoding-server-<triple>
# 是否存在。本地开发若未先构建 minicoding-server，需运行此脚本创建占位文件。
#
# 真实发布构建请用 scripts/build-desktop.sh（会构建真实 server 二进制并覆盖占位）。
#
# 用法：./scripts/setup-desktop-dev.sh

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BINARIES_DIR="$ROOT_DIR/crates/minicoding-desktop/binaries"
mkdir -p "$BINARIES_DIR"

# 获取 host target triple
TARGET="$(rustc -vV | sed -n 's/^host: //p')"

BIN_NAME="minicoding-server-$TARGET"
if [[ "$TARGET" == *"-windows-"* ]]; then
  BIN_NAME="$BIN_NAME.exe"
fi

BIN_PATH="$BINARIES_DIR/$BIN_NAME"

if [[ -f "$BIN_PATH" ]]; then
  echo "✓ sidecar 占位已存在：$BIN_PATH"
  exit 0
fi

# 创建占位 shell 脚本（仅满足 tauri_build 校验，不可实际运行）
cat > "$BIN_PATH" <<'EOF'
#!/bin/sh
# 占位 sidecar（开发模式）。真实二进制请用 scripts/build-desktop.sh 构建。
echo "listening on 127.0.0.1:8080"
sleep 999999
EOF
chmod +x "$BIN_PATH"

echo "✓ 已创建 sidecar 占位：$BIN_PATH"
echo "  开发模式 cargo build -p minicoding-desktop --features desktop 现可正常编译。"
echo "  发布构建请运行：./scripts/build-desktop.sh"
