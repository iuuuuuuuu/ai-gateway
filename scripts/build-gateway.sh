#!/bin/sh
# 构建**Rust 版**网关，产物落到 crates/ai-gateway-core/embedded/，
# 之后 cargo build 时 build.rs 会把它 gzip 压缩后编进主程序。
#
# 与 scripts/build-gateway.ps1 等价（Windows 用 .ps1，CI / Unix 用本脚本）。
# Go 版实现保留在 scripts/build-gateway-go.sh，供回退与 A/B 对照使用。
#
# 产物路径与 Go 版**完全相同**，因此宿主侧（gateway.rs 的 resolve_gateway_exe、
# build.rs 的 candidate_paths）不需要任何改动 —— 换网关实现只换构建来源。
#
# 保持 POSIX sh 兼容：CI（Ubuntu）用 `sh` 调用时是 dash，不支持 pipefail
# 等 bash 扩展。与 make-dmg.sh 遵循同一约定。
#
# 用法：
#   sh scripts/build-gateway.sh              # release 构建
#   DEV_BUILD=1 sh scripts/build-gateway.sh  # debug 构建（快，便于调试）
set -eu

cd "$(dirname "$0")/.."
ROOT=$PWD

# cargo 兜底：部分 CI 镜像里 cargo 不在 PATH（rustup 装在 ~/.cargo 下）。
if ! command -v cargo >/dev/null 2>&1; then
  if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "build-gateway: 未找到 cargo，请先安装 Rust 工具链" >&2
  exit 1
fi

OUT_DIR="$ROOT/crates/ai-gateway-core/embedded"
mkdir -p "$OUT_DIR"

PROFILE=release
CARGO_FLAGS="--release"
if [ "${DEV_BUILD:-}" = "1" ]; then
  PROFILE=dev
  CARGO_FLAGS=""
fi

# 交叉编译：release 矩阵给每个平台设了 target 三元组。
# 用 CARGO_BUILD_TARGET 而不是 --target，这样下面的产物路径判断只需看
# 环境变量，且与 cargo 自身的约定一致（cargo 会把它写进 target/<triple>/）。
if [ -n "${CARGO_BUILD_TARGET:-}" ]; then
  TARGET_DIR="$ROOT/target/$CARGO_BUILD_TARGET/$PROFILE"
else
  TARGET_DIR="$ROOT/target/$PROFILE"
fi

echo "==> 构建 Rust 网关（$PROFILE${CARGO_BUILD_TARGET:+, target=$CARGO_BUILD_TARGET}）"
# shellcheck disable=SC2086
cargo build -p ai-gateway-router --bin gateway $CARGO_FLAGS

# Windows 产物带 .exe；macOS/Linux 不带。build.rs 按同样规则查找。
#
# 判据用 target 三元组（交叉编译时 `uname -s` 是**构建机**的系统，不可信），
# 没有三元组时才回落 uname。
case "${CARGO_BUILD_TARGET:-$(uname -s)}" in
  *windows*|MINGW*|MSYS*|CYGWIN*)
    BUILT="$TARGET_DIR/gateway.exe"
    OUT="$OUT_DIR/gateway.exe"
    ;;
  *)
    BUILT="$TARGET_DIR/gateway"
    OUT="$OUT_DIR/gateway"
    ;;
esac

if [ ! -f "$BUILT" ]; then
  echo "build-gateway: 未找到构建产物 $BUILT" >&2
  exit 1
fi

cp "$BUILT" "$OUT"
SIZE=$(wc -c < "$OUT" | tr -d ' ')
if [ "$SIZE" -lt 1048576 ]; then
  echo "build-gateway: 产物异常（仅 $SIZE 字节）" >&2
  exit 1
fi
echo "==> 完成: $OUT ($((SIZE / 1048576)) MB)"
