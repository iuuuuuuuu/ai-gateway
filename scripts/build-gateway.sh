#!/bin/sh
# 构建内嵌网关（Go），产物落到 crates/wb-switch-core/embedded/，
# 之后 cargo build 时 build.rs 会把它 gzip 压缩后编进主程序。
#
# 保持 POSIX sh 兼容：CI（Ubuntu）用 `sh` 调用时是 dash，不支持 pipefail
# 等 bash 扩展。与 make-dmg.sh 遵循同一约定。
#
# 网关源码随仓库分发在 go-gateway/，直接本地构建，不再 clone 上游、不再打补丁。
#
# 用法：
#   sh scripts/build-gateway.sh                      # 按当前平台构建
#   GOOS=windows GOARCH=amd64 sh scripts/build-gateway.sh
#   GOOS=darwin  GOARCH=arm64 sh scripts/build-gateway.sh
#
# 环境变量：
#   GOOS / GOARCH         目标平台，缺省取 `go env`
#   GOFLAGS               传给 go build，缺省 -mod=mod
set -eu

cd "$(dirname "$0")/.."
ROOT=$PWD

SRC="$ROOT/go-gateway"
if [ ! -d "$SRC/cmd/server" ]; then
  echo "build-gateway: 未找到网关源码 $SRC/cmd/server" >&2
  exit 1
fi

GOOS="${GOOS:-$(go env GOOS)}"
GOARCH="${GOARCH:-$(go env GOARCH)}"

# Windows 用 .exe 后缀；macOS/Linux 不带后缀。build.rs 按同样规则查找。
if [ "$GOOS" = "windows" ]; then
  OUT="$ROOT/crates/wb-switch-core/embedded/gateway.exe"
else
  OUT="$ROOT/crates/wb-switch-core/embedded/gateway"
fi

echo "==> 构建 $GOOS/$GOARCH"
mkdir -p "$(dirname "$OUT")"
(
  cd "$SRC"
  # CGO_ENABLED=0：纯静态链接，交叉编译 macOS/Windows 时不依赖目标平台 C 工具链。
  CGO_ENABLED=0 GOOS="$GOOS" GOARCH="$GOARCH" GOFLAGS="${GOFLAGS:--mod=mod}" \
    go build -trimpath -ldflags "-s -w" -o "$OUT" ./cmd/server
)

SIZE=$(wc -c < "$OUT" | tr -d ' ')
if [ "$SIZE" -lt 1048576 ]; then
  echo "build-gateway: 产物异常（仅 $SIZE 字节）" >&2
  exit 1
fi
echo "==> 完成: $OUT ($((SIZE / 1048576)) MB)"
