#!/bin/bash
# 生成 tauri updater 版本清单 latest-<os>-<arch>.json（Windows-only）
# 用法：
#   UPDATE_OS=windows UPDATE_ARCH=x86_64 sh scripts/gen-update-json.sh [owner] [repo]
# 可选：BUNDLE_DIR、UPDATE_ARCHIVE_NAME
set -e
cd "$(dirname "$0")/.." || exit 1

# 默认值必须与 src-tauri/tauri.conf.json 的 updater endpoints 一致，
# 否则生成的 URL 会指向别的仓库，客户端永远拉不到更新。
#
# 注意：应用显示名是 AI Gateway。**本仓库是 fork**，更新源必须是本 fork ——
# 指向上游（momo0410/ai-gateway）会把用户更新成上游版本，本 fork 的功能全部消失。
# releases/latest 在每个 fork 上都是**不同的指针**，所以这里写错不是 404 就是切错版本。
OWNER="${1:-iuuuuuuuu}"
REPO="${2:-ai-gateway}"
VERSION="${UPDATE_VERSION:-$(grep '^version' src-tauri/Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')}"
UPDATE_OS="${UPDATE_OS:-windows}"
UPDATE_ARCH="${UPDATE_ARCH:-x86_64}"
UPDATE_ARCHIVE_NAME="${UPDATE_ARCHIVE_NAME:-}"

case "$UPDATE_ARCH" in
  aarch64|x86_64) ;;
  *)
    echo "gen-update-json: 不支持的架构：$UPDATE_ARCH（只支持 aarch64 或 x86_64）" >&2
    exit 1
    ;;
esac

case "$UPDATE_OS" in
  windows)
    DEFAULT_BUNDLE="nsis"
    # Tauri 2 createUpdaterArtifacts=true 签的是当前版本安装包：*_VERSION_x64-setup.exe.sig
    SIG_GLOB="*_${VERSION}_x64-setup.exe.sig"
    PLATFORM_KEYS="windows-$UPDATE_ARCH-nsis windows-$UPDATE_ARCH"
    ;;
  darwin)
    # ⚠ macOS 的更新包是 **.app.tar.gz**，不是 .dmg ——
    # Tauri 的 updater 只能替换 .app bundle，dmg 是给用户手动安装用的。
    # 签名由 `tauri build --bundles app` 产出（签的是那个 tar.gz）。
    DEFAULT_BUNDLE="macos"
    SIG_GLOB="*.app.tar.gz.sig"
    # bundle 后缀是 `app`（见 tauri-plugin-updater 的 bundle_type）。
    PLATFORM_KEYS="darwin-$UPDATE_ARCH-app darwin-$UPDATE_ARCH"
    ;;
  linux)
    # Linux 用 AppImage（自更新友好）。签名文件是 *.AppImage.tar.gz.sig。
    DEFAULT_BUNDLE="appimage"
    SIG_GLOB="*.AppImage.tar.gz.sig"
    PLATFORM_KEYS="linux-$UPDATE_ARCH-appimage linux-$UPDATE_ARCH"
    ;;
  *)
    echo "gen-update-json: 不支持的系统：$UPDATE_OS（支持 windows / darwin / linux）" >&2
    exit 1
    ;;
esac

BUNDLE_DIR="${BUNDLE_DIR:-target/release/bundle/$DEFAULT_BUNDLE}"
if [ ! -d "$BUNDLE_DIR" ] && [ -d "src-tauri/target/release/bundle/$DEFAULT_BUNDLE" ]; then
  BUNDLE_DIR="src-tauri/target/release/bundle/$DEFAULT_BUNDLE"
fi

SIG_MATCHES=$(find "$BUNDLE_DIR" -maxdepth 2 -type f -name "$SIG_GLOB" | sort)
SIG_COUNT=$(printf '%s\n' "$SIG_MATCHES" | sed '/^$/d' | wc -l | tr -d ' ')
if [ "$SIG_COUNT" != 1 ]; then
  echo "gen-update-json: 期望恰好 1 个签名包（$SIG_GLOB），实际 $SIG_COUNT" >&2
  printf '%s\n' "$SIG_MATCHES" >&2
  ls -la "$BUNDLE_DIR" >&2 || true
  exit 1
fi
SIG_FILE=$SIG_MATCHES
ARCHIVE_FILE="${SIG_FILE%.sig}"
JSON_FILE="$BUNDLE_DIR/latest-$UPDATE_OS-$UPDATE_ARCH.json"

if [ ! -f "$SIG_FILE" ] || [ ! -f "$ARCHIVE_FILE" ]; then
  echo "gen-update-json: 未找到签名更新包（$SIG_GLOB in $BUNDLE_DIR）" >&2
  ls -la "$BUNDLE_DIR" >&2 || true
  exit 1
fi

SIGNATURE=$(cat "$SIG_FILE")
if [ -n "$UPDATE_ARCHIVE_NAME" ]; then
  ARCHIVE_NAME="$UPDATE_ARCHIVE_NAME"
else
  ARCHIVE_NAME=$(basename "$ARCHIVE_FILE")
fi
PUB_DATE=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
URL="https://github.com/$OWNER/$REPO/releases/latest/download/$ARCHIVE_NAME"

PLATFORMS=""
for key in $PLATFORM_KEYS; do
  [ -n "$PLATFORMS" ] && PLATFORMS="$PLATFORMS,"
  PLATFORMS="$PLATFORMS
    \"$key\": {
      \"signature\": \"$SIGNATURE\",
      \"url\": \"$URL\"
    }"
done

mkdir -p "$BUNDLE_DIR"
cat > "$JSON_FILE" << EOF
{
  "version": "$VERSION",
  "notes": "",
  "pub_date": "$PUB_DATE",
  "platforms": {$PLATFORMS
  }
}
EOF
echo "gen-update-json: 已生成 $JSON_FILE"
