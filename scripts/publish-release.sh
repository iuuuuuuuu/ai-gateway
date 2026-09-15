#!/bin/bash
# 发布 GitHub Release 资产：签名固定名安装包 + 生成 updater 清单 + 上传
#
# 为什么需要这个脚本：
#   tauri updater 客户端从 releases/latest/download/latest.json 拉取版本清单，
#   清单里的 signature 是对「下载 URL 对应的那个文件名」做的签名。
#   安装包在原始 bundle 目录里叫 ai-gateway_<版本>_x64-setup.exe，
#   而 Release 上为了固定 URL 重命名为 ai-gateway-windows-x86_64-setup.exe，
#   两者字节相同但文件名不同 —— 必须对重命名后的文件重新签名，否则客户端校验失败。
#
# 用法：
#   TAURI_SIGNING_PRIVATE_KEY_PASSWORD=<密码> sh scripts/publish-release.sh
#   可选环境变量：
#     TAG=<tag>            指定 Release tag，默认取最新 tag
#     SKIP_UPLOAD=1        只生成文件不上传（dry-run）
#
# 私钥来源（按优先级）：
#   1. 环境变量 TAURI_SIGNING_PRIVATE_KEY（内容或路径）
#   2. ~/.ai-gateway/ai-gateway-updater.key
set -e
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT" || exit 1

OWNER="${RELEASE_OWNER:-momo0410}"
# 仓库名与应用显示名不同：显示名是 AI Gateway，GitHub 仓库仍是
# workbuddy-switch-gateway（未改名）。这里写错会让自动更新 404。
REPO="${RELEASE_REPO:-workbuddy-switch-gateway}"
SETUP_NAME="ai-gateway-windows-x86_64-setup.exe"
WORK_DIR="$REPO_ROOT/.release-staging"

# ── 1. 解析版本与 tag ─────────────────────────────────────────────
TAG="${TAG:-$(git tag --sort=-v:refname | head -1)}"
[ -z "$TAG" ] && { echo "错误：未找到任何 git tag" >&2; exit 1; }
VER="${TAG#v}"
# 校验 tag 与 Cargo.toml 版本一致，防止清单版本号与实际产物错位
CARGO_VER=$(grep '^version' src-tauri/Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
if [ "$VER" != "$CARGO_VER" ]; then
  echo "错误：tag 版本 $VER 与 src-tauri/Cargo.toml 的 $CARGO_VER 不一致" >&2
  exit 1
fi
echo "发布目标：$TAG（版本 $VER）"

# ── 2. 定位签名私钥 ──────────────────────────────────────────────
KEY_FILE="${HOME}/.ai-gateway/ai-gateway-updater.key"
if [ -z "$TAURI_SIGNING_PRIVATE_KEY" ] && [ -f "$KEY_FILE" ]; then
  TAURI_SIGNING_PRIVATE_KEY="$(cat "$KEY_FILE")"
  export TAURI_SIGNING_PRIVATE_KEY
fi
if [ -z "$TAURI_SIGNING_PRIVATE_KEY" ]; then
  echo "错误：未找到签名私钥（设置 TAURI_SIGNING_PRIVATE_KEY 或放置于 $KEY_FILE）" >&2
  exit 1
fi
if [ -z "$TAURI_SIGNING_PRIVATE_KEY_PASSWORD" ]; then
  echo "错误：未设置 TAURI_SIGNING_PRIVATE_KEY_PASSWORD" >&2
  exit 1
fi

# ── 3. 准备待签名文件：优先本地构建产物，其次从 Release 取回 ─────
# 首次发布新版本时 Release 上还没有该文件，所以必须能从本地 bundle 取。
mkdir -p "$WORK_DIR"
SRC_SETUP=""
for cand in \
  "$REPO_ROOT/target/release/bundle/nsis/ai-gateway_${VER}_x64-setup.exe" \
  "$REPO_ROOT/target/x86_64-pc-windows-msvc/release/bundle/nsis/ai-gateway_${VER}_x64-setup.exe" \
  "$REPO_ROOT/src-tauri/target/release/bundle/nsis/ai-gateway_${VER}_x64-setup.exe" \
  "$REPO_ROOT/target/release/bundle/nsis/AI Gateway_${VER}_x64-setup.exe"; do
  [ -f "$cand" ] && { SRC_SETUP="$cand"; break; }
done

cd "$WORK_DIR"
if [ -n "$SRC_SETUP" ]; then
  cp "$SRC_SETUP" "$SETUP_NAME"
  echo "已从本地构建产物复制：$SRC_SETUP"
elif [ -f "$SETUP_NAME" ]; then
  echo "复用暂存目录中已存在的 $SETUP_NAME"
else
  echo "本地无构建产物，从 $TAG 下载 $SETUP_NAME ..."
  gh release download "$TAG" --pattern "$SETUP_NAME" --clobber
fi
SETUP_SHA=$(sha256sum "$SETUP_NAME" | cut -d' ' -f1)
echo "安装包 SHA256: $SETUP_SHA"

# ── 4. 对固定名文件签名 ──────────────────────────────────────────
echo "签名 $SETUP_NAME ..."
npx tauri signer sign "$SETUP_NAME"

# ── 5. 生成 latest.json ─────────────────────────────────────────
URL="https://github.com/$OWNER/$REPO/releases/latest/download/$SETUP_NAME"
SIG=$(cat "$SETUP_NAME.sig")
# pub_date 取 tag 指向提交的 UTC 时间，保证格式规范且单调递增
PUB_DATE=$(git log -1 --date=format:'%Y-%m-%dT%H:%M:%SZ' --format=%cd "$TAG")
python3 - "$VER" "$SIG" "$URL" "$PUB_DATE" <<'PY'
import json, sys
ver, sig, url, pub_date = sys.argv[1:5]
manifest = {
    "version": ver,
    "notes": "",
    "pub_date": pub_date,
    "platforms": {
        # nsis 是 Tauri 2 的 windows target 键；裸键兼容旧客户端读取逻辑
        "windows-x86_64-nsis": {"signature": sig, "url": url},
        "windows-x86_64": {"signature": sig, "url": url},
    },
}
with open("latest.json", "w", encoding="utf-8") as f:
    json.dump(manifest, f, indent=2, ensure_ascii=False)
    f.write("\n")
print("已生成 latest.json")
PY

# ── 6. 本地校验：签名 key id 必须与配置公钥一致 ──────────────────
python3 - "$REPO_ROOT/src-tauri/tauri.conf.json" <<'PY'
import base64, json, sys

cfg = json.load(open(sys.argv[1], encoding="utf-8"))
pub_line = base64.b64decode(cfg["plugins"]["updater"]["pubkey"]).decode().strip().split("\n")[1]
pub_id = base64.b64decode(pub_line)[2:10].hex()

manifest = json.load(open("latest.json", encoding="utf-8"))
ok = True
for plat, info in manifest["platforms"].items():
    sig_line = base64.b64decode(info["signature"]).decode().strip().split("\n")[1]
    sid = base64.b64decode(sig_line)[2:10].hex()
    if sid != pub_id:
        print(f"  校验失败 [{plat}]：签名 keyid {sid} != 公钥 keyid {pub_id}", file=sys.stderr)
        ok = False
    else:
        print(f"  校验通过 [{plat}] keyid {sid}")
if not ok:
    print("错误：签名与 tauri.conf.json 公钥不匹配，客户端将无法更新", file=sys.stderr)
    sys.exit(1)
print(f"签名校验通过（公钥 keyid {pub_id}）")
PY

# ── 7. 上传 ─────────────────────────────────────────────────────
if [ "${SKIP_UPLOAD:-}" = "1" ]; then
  echo "SKIP_UPLOAD=1，跳过上传。产物位于 $WORK_DIR/"
  exit 0
fi
echo "上传到 $TAG ..."
gh release upload "$TAG" latest.json "$SETUP_NAME.sig" --clobber

# ── 8. 上传后验证端点真实可达 ────────────────────────────────────
echo "验证 updater 端点 ..."
HTTP=$(curl -sSL -o /dev/null -w '%{http_code}' \
  "https://github.com/$OWNER/$REPO/releases/latest/download/latest.json")
if [ "$HTTP" != "200" ]; then
  echo "错误：端点返回 HTTP $HTTP" >&2
  exit 1
fi
echo "完成：$TAG 的 latest.json 已发布，端点返回 200"
echo "  $URL"
echo "  安装包 SHA256: $SETUP_SHA"
