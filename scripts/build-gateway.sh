#!/bin/sh
# 构建内嵌网关（**Rust**），产物落到 crates/ai-gateway-core/embedded/，
# 之后 cargo build 时 build.rs 会把它 gzip 压缩后编进主程序。
#
# # 为什么从 Go 换成 Rust（2026-10-01，所有者决策）
#
# 所有者原话：
#
#	「我们也换成rust全重写」
#
# 网关内核已完整移植到 `crates/ai-gateway-router`（Rust），与 Go 版逐行对齐、
# 实机自测通过。本脚本随之切换：不再需要 Go 工具链，CI 也不用装 setup-go。
#
# Go 源码 `go-gateway/` **仍然保留在仓库里**，作为移植的对照参考与回归依据
#（Rust 侧的注释大量逐字引用 `internal/**` 的行号），不是死代码。
#
# # 为什么不再交叉编译
#
# 旧版用 `GOOS=windows GOARCH=amd64` 从 Linux/macOS 交叉编出三平台网关。
# Rust 侧**刻意不做**这件事：交叉编译要装目标三元组的 std 与链接器
#（`rustup target add` + `cargo build --target`），而本脚本的调用点全都在
# 「为当前平台出包」的上下文里 —— CI 的 release 矩阵本来就是
# windows-latest / macos-14 / macos-15-intel / ubuntu-22.04 各自跑一遍，
# 每腿只需要本机产物。加交叉编译只会引入一类「目标三元组没装」的新故障，
# 换不来任何东西。
#
# 于是 `GOOS`/`GOARCH` 这两个环境变量**不再被读取**（若外部仍传，只打一行
# 提示后忽略，不报错 —— 避免旧调用点突然失败）。
#
# ★ 因为构建不带 `--target`，产物落在 `target/<profile>/`，而**不是**
#   `target/<triple>/<profile>/`。上面那四个 CI 腿的 host 与 target 恰好
#   一致，所以这是对的；若将来出现真正的交叉腿，必须回来加 `--target`。
#
# # 用法
#
#   sh scripts/build-gateway.sh                       # 本机 release 构建
#   CARGO_PROFILE=debug sh scripts/build-gateway.sh   # debug（仅排障用）
#   CARGO_ARGS=--offline sh scripts/build-gateway.sh  # 额外 cargo 参数
#
# # 环境变量
#
#   CARGO_PROFILE   缺省 release。debug 产物约 22 MB，会显著撑大安装包，
#                   只用于「网关起不来、要看 panic 回溯」的排障场景。
#   CARGO           缺省 cargo；受限沙箱下 cargo 不在 PATH 时可显式指定。
#   CARGO_ARGS      额外传给 `cargo build` 的参数（按空格切分）。
#                   受限网络下用 `CARGO_ARGS=--offline` 走本地缓存。
set -eu

cd "$(dirname "$0")/.."
ROOT=$PWD

CRATE_DIR="$ROOT/crates/ai-gateway-router"
if [ ! -f "$CRATE_DIR/src/bin/gateway.rs" ]; then
  echo "build-gateway: 未找到网关源码 $CRATE_DIR/src/bin/gateway.rs" >&2
  exit 1
fi

CARGO="${CARGO:-cargo}"
PROFILE="${CARGO_PROFILE:-release}"

case "$PROFILE" in
  release) PROFILE_FLAG="--release" ;;
  debug)   PROFILE_FLAG="" ;;
  *)
    echo "build-gateway: CARGO_PROFILE 只支持 release / debug（收到 ${PROFILE}）" >&2
    exit 1
    ;;
esac

# 旧调用点会传这两个变量。显式提示一次，比静默忽略更容易排查
#「为什么我指定的目标平台没生效」。
if [ -n "${GOOS:-}" ] || [ -n "${GOARCH:-}" ]; then
  echo "build-gateway: 已忽略 GOOS/GOARCH（Rust 版按构建主机出产物，见脚本头部说明）" >&2
fi

# 产物后缀按**构建主机**判定，与 crates/ai-gateway-core/build.rs 的
# `if cfg!(windows) { "gateway.exe" } else { "gateway" }` 必须一致 ——
# 两处分叉会让 build.rs 找不到产物而**静默不内嵌**（应用能起，网关功能报
# 「本构建未内嵌网关二进制」）。
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) HOST_OS=windows ;;
  *)                    HOST_OS=unix ;;
esac
if [ "$HOST_OS" = "windows" ]; then
  BIN_NAME="gateway.exe"
else
  BIN_NAME="gateway"
fi
OUT="$ROOT/crates/ai-gateway-core/embedded/$BIN_NAME"

echo "==> 构建 Rust 网关（${PROFILE}，$(uname -s)/$(uname -m)）"
mkdir -p "$(dirname "$OUT")"

# shellcheck disable=SC2086  # CARGO_ARGS 有意按空格切分
"$CARGO" build -p ai-gateway-router --bin gateway $PROFILE_FLAG ${CARGO_ARGS:-} || {
  echo "build-gateway: cargo build 失败。" >&2
  echo "  若报错是「failed to download / no matching package」类，那是网络问题：" >&2
  echo "    · 本机可用 .cargo/config.toml 把 crates-io 换到可达镜像；" >&2
  echo "    · 依赖已进本地缓存时，用 CARGO_ARGS=--offline 重跑。" >&2
  exit 1
}

# cargo 的产物目录名与 profile 同名（release / debug）。CARGO_TARGET_DIR
# 可被外部改写，故先按它算，相对路径按仓库根归一。
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
case "$TARGET_DIR" in
  /*) ;;
  *) TARGET_DIR="$ROOT/$TARGET_DIR" ;;
esac
SRC_BIN="$TARGET_DIR/$PROFILE/$BIN_NAME"
if [ ! -f "$SRC_BIN" ]; then
  echo "build-gateway: 未找到构建产物 $SRC_BIN" >&2
  exit 1
fi

cp -f "$SRC_BIN" "$OUT"
chmod +x "$OUT" 2>/dev/null || true

SIZE=$(wc -c < "$OUT" | tr -d ' ')
# 阈值与 build.rs 的「第一个 >1 MB 的候选胜出」一致：低于它 build.rs 会判为
# 「不是有效网关」而跳过内嵌，静默产出不内嵌网关的包。
if [ "$SIZE" -lt 1048576 ]; then
  echo "build-gateway: 产物异常（仅 $SIZE 字节，低于 build.rs 的 1 MB 阈值）" >&2
  exit 1
fi
echo "==> 完成: $OUT ($((SIZE / 1048576)) MB)"
