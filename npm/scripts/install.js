// ai-gateway postinstall：从「平台包」复制本平台二进制。
//
// 二进制发布在独立平台包（ai-gateway-<platform>-<arch>），主包在
// optionalDependencies 里声明，安装时 npm 自动装好本机对应的那个，postinstall 只需复制——
// 不依赖 GitHub，国内镜像（npmmirror）也能稳定安装。
//
// 支持的平台见下方 FILE 映射。注意 optionalDependencies 是「按 os/cpu 字段择优安装」，
// 平台包必须同时声明 os 与 cpu，否则在别的架构上也会被装上。
//
// 环境变量覆盖：
//   AI_GATEWAY_BINARY=<本地二进制路径>  本地开发/离线安装（直接复制，不联网）
const fs = require("fs");
const path = require("path");

// 平台 → 平台包内二进制文件名。
//
// macOS 有两个架构：Intel(x64) 与 Apple Silicon(arm64)，二者二进制不通用，
// 必须分别发布（release 工作流里对应 macos-x64 与 macos-arm64 两个 job）。
const FILE = {
  "win32-x64": "ai-gateway-win32-x64.exe",
  "darwin-x64": "ai-gateway-darwin-x64",
  "darwin-arm64": "ai-gateway-darwin-arm64",
  "linux-x64": "ai-gateway-linux-x64",
  "linux-arm64": "ai-gateway-linux-arm64",
}[`${process.platform}-${process.arch}`];

const PLATFORM_PKG = `ai-gateway-${process.platform}-${process.arch}`;

if (!FILE) {
  console.warn(
    `ai-gateway: 跳过平台 ${process.platform}-${process.arch}（暂无对应平台包），` +
      `可设置 AI_GATEWAY_BINARY 指向本地二进制，或手动放置到 bin/ 目录`,
  );
  process.exit(0);
}

const binDir = path.join(__dirname, "..", "bin");
const target = path.join(binDir, FILE);

function fail(msg) {
  console.error(`ai-gateway install: ${msg}`);
  console.error(
    "安装失败。请确认安装了对应平台包（npm 会自动装），或设置 AI_GATEWAY_BINARY 指向本地二进制。",
  );
  process.exit(1);
}

function copyFrom(src) {
  fs.mkdirSync(binDir, { recursive: true });
  fs.copyFileSync(src, target);
  if (process.platform !== "win32") fs.chmodSync(target, 0o755);
  const size = fs.statSync(target).size;
  if (size < 1 * 1024 * 1024) {
    fs.unlinkSync(target);
    return fail(`平台包二进制异常（仅 ${size} 字节）`);
  }
  console.log(
    `ai-gateway: 二进制就绪 → ${target} (${(size / 1048576).toFixed(1)}MB)`,
  );
}

async function main() {
  // 1) 本地二进制覆盖（开发/离线）
  if (process.env.AI_GATEWAY_BINARY) {
    const local = path.resolve(process.env.AI_GATEWAY_BINARY);
    if (fs.existsSync(local)) return copyFrom(local);
    return fail(`AI_GATEWAY_BINARY 指向的文件不存在: ${local}`);
  }

  // 2) 从平台包复制（node_modules/ai-gateway-<platform>-<arch>/bin/<file>）
  try {
    const pkgRoot = path.dirname(require.resolve(`${PLATFORM_PKG}/package.json`));
    const src = path.join(pkgRoot, "bin", FILE);
    if (fs.existsSync(src)) return copyFrom(src);
    return fail(`平台包 ${PLATFORM_PKG} 中未找到 ${FILE}`);
  } catch (e) {
    // 平台包缺失：可能是 optionalDependencies 没装上（如手动安装/旧版 npm）
    if (fs.existsSync(target)) {
      console.log("ai-gateway: 二进制已存在，跳过");
      return;
    }
    return fail(
      `平台包 ${PLATFORM_PKG} 未安装（${e.message}）。请重新执行 npm install。`,
    );
  }
}

main();
