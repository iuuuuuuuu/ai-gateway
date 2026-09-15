#!/bin/bash
# 统一 bump 版本：usage: sh scripts/bump-version.sh 0.1.5
#
# 不依赖 perl：Windows 的 Git Bash 默认不带 perl，用 sed 等价替换。
# 本脚本是本地发版辅助工具，CI 的 release.yml 不调用它。
set -e
V=$1
[ -z "$V" ] && echo "用法: sh scripts/bump-version.sh <新版本>" && exit 1
cd "$(dirname "$0")/.."

# 版本号必须是 x.y.z：写进各清单文件之前先校验，避免空值/非法值混入产物
printf '%s' "$V" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$' || {
  echo "版本号格式应为 x.y.z，收到：$V" >&2
  exit 1
}

# JSON 清单里的 "version": "x.y.z"
sed -i -E "s/\"version\": \"[0-9]+\.[0-9]+\.[0-9]+\"/\"version\": \"$V\"/" \
  package.json src-tauri/tauri.conf.json npm/package.json

# 各 Cargo.toml 顶层的 version = "x.y.z"
sed -i -E "s/^version = \"[0-9]+\.[0-9]+\.[0-9]+\"/version = \"$V\"/" \
  src-tauri/Cargo.toml crates/ai-gateway-core/Cargo.toml \
  crates/ai-gateway-server/Cargo.toml crates/ai-gateway-router/Cargo.toml

# 平台包 package.json
for f in npm/platform/*/package.json; do
  sed -i -E "s/\"version\": \"[0-9]+\.[0-9]+\.[0-9]+\"/\"version\": \"$V\"/" "$f"
done

# Cargo.lock 里 workspace 各 crate 的 version（只替换紧随包名之后的那一行）。
# 必须锚定包名：Cargo.lock 里 bit-set / bit-vec / ctor 等第三方 crate 的版本号
# 也恰好是 0.8.0 这类值，一律全局替换会误伤依赖锁。
sed -i -E "/^name = \"ai-gateway-(core|gateway|rust|server)\"/{n;s/^version = \"[0-9]+\.[0-9]+\.[0-9]+\"/version = \"$V\"/}" Cargo.lock

# 主包 optionalDependencies 引用版本（用 node 解析 JSON，避免正则误伤）
node -e "
const fs = require('fs');
const p = 'npm/package.json';
const j = JSON.parse(fs.readFileSync(p));
for (const k of Object.keys(j.optionalDependencies || {})) j.optionalDependencies[k] = '$V';
fs.writeFileSync(p, JSON.stringify(j, null, 2) + '\n');
"
echo "所有版本已同步为 $V（含平台包、Cargo.lock 与 optionalDependencies）"
