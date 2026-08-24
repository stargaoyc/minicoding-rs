#!/usr/bin/env bash
# 桌面应用构建脚本（本地开发 + CI 共用）。
#
# 流程：
#   1. 构建 Web 前端（pnpm install --frozen-lockfile + pnpm run build → crates/minicoding-web/dist/）
#   2. 构建 minicoding-server 二进制（cargo build --release -p minicoding-server）
#   3. 将 server 二进制复制到 crates/minicoding-desktop/binaries/minicoding-server-<triple>
#      （Tauri 2.x externalBin 约定：自动追加 host target triple 后缀）
#   4. 调用 `cargo tauri build` 产出平台安装包（.dmg / .msi / .AppImage）
#
# 用法：
#   ./scripts/build-desktop.sh                # 用 host target 构建
#   ./scripts/build-desktop.sh aarch64-apple-darwin  # 交叉编译指定 target
#
# 依赖：
#   - Node.js + pnpm（CI 经 pnpm/action-setup 安装，本地需自备）
#   - Rust toolchain
#   - cargo-tauri（`cargo install tauri-cli --version "^2"`）
#   - 平台 GUI 系统库（Linux: webkit2gtk-4.1/glib/dbus；macOS/Windows 自带）

set -euo pipefail

# 项目根目录（脚本可从任意 cwd 调用）
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

# 目标 triple（默认 host target）
TARGET="${1:-}"

# 探测 cargo-tauri
if ! command -v cargo-tauri >/dev/null 2>&1; then
  echo "❌ cargo-tauri 未安装。请运行：cargo install tauri-cli --version '^2'" >&2
  exit 1
fi

# 探测 pnpm（与 CI 一致：pnpm/action-setup 安装后 PATH 可用）
if ! command -v pnpm >/dev/null 2>&1; then
  echo "❌ pnpm 未安装" >&2
  exit 1
fi

echo "==> [1/4] 构建 Web 前端（crates/minicoding-web）"
(
  cd crates/minicoding-web
  pnpm install --frozen-lockfile
  pnpm run build
)
echo "✓ 前端构建完成：crates/minicoding-web/dist/"

echo "==> [2/4] 构建 minicoding-server 二进制"
CARGO_BUILD_ARGS=(--release -p minicoding-server)
if [[ -n "$TARGET" ]]; then
  CARGO_BUILD_ARGS+=(--target "$TARGET")
fi
cargo build "${CARGO_BUILD_ARGS[@]}"

# 解析实际 target triple（未指定时取 host triple）
if [[ -z "$TARGET" ]]; then
  TARGET="$(rustc -vV | sed -n 's/^host: //p')"
fi
echo "✓ server 二进制构建完成（target: $TARGET）"

echo "==> [3/4] 放置 sidecar 二进制（Tauri externalBin 约定）"
DESKTOP_DIR="crates/minicoding-desktop"
BINARIES_DIR="$DESKTOP_DIR/binaries"
mkdir -p "$BINARIES_DIR"

# 查找构建产物路径
if [[ -n "$1" ]]; then
  # 显式指定 target 时二进制在 target/<target>/release/
  SERVER_BIN="target/$TARGET/release/minicoding-server"
else
  # host target 时二进制在 target/release/
  SERVER_BIN="target/release/minicoding-server"
fi

# Windows 二进制带 .exe 后缀
# 注意：sidecar 文件名必须与 tauri.conf.json 的 externalBin 一致（minicoding-server-sidecar），
# 不能直接用 minicoding-server（会与 cargo 的 build 目录 target/release/build/minicoding-server/ 冲突，
# 导致 tauri-build 的 copy_binaries 函数 fs::remove_file 时 IsADirectory panic）
SIDECAR_NAME="minicoding-server-sidecar-$TARGET"
if [[ "$TARGET" == *"-windows-"* ]]; then
  SIDECAR_NAME="$SIDECAR_NAME.exe"
  SERVER_BIN="$SERVER_BIN.exe"
fi

if [[ ! -f "$SERVER_BIN" ]]; then
  echo "❌ server 二进制未找到：$SERVER_BIN" >&2
  exit 1
fi

cp "$SERVER_BIN" "$BINARIES_DIR/$SIDECAR_NAME"
chmod +x "$BINARIES_DIR/$SIDECAR_NAME"
echo "✓ sidecar 已放置：$BINARIES_DIR/$SIDECAR_NAME"

echo "==> [4/4] 调用 cargo tauri build（产出平台安装包）"
TAURI_ARGS=(build --features desktop)
if [[ -n "$1" ]]; then
  TAURI_ARGS+=(--target "$TARGET")
fi
(
  cd "$DESKTOP_DIR"
  cargo tauri "${TAURI_ARGS[@]}"
)

echo ""
echo "🎉 桌面应用构建完成！"
echo "   安装包位置：target/${TARGET}/release/bundle/（workspace target 目录）"
echo "   - macOS:  .dmg / .app"
echo "   - Windows: .msi / .exe"
echo "   - Linux:  .AppImage / .deb"
