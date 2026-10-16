// turing-sdk-shim.cjs —— TuringShield SDK 的**最小加载垫片**。
//
// # 为什么需要它（2026-10-16 现场）
//
// 官方客户端里真正的 SDK 是**两个原生文件**：
//
//   build/Release/turing_sdk.node      158 KB   N-API 原生模块
//   build/Release/TuringShieldSDK.dll  1.44 MB  腾讯的 DLL
//
// 而客户端目录里**还有一个** `index.cjs`（1.2 KB）—— 那只是 require 一下
// 原生模块的 JS 垫片，本身**不含任何 SDK 逻辑**：
//
//   binding = require(path.join(__dirname, 'build', 'Release', 'turing_sdk.node'))
//
// # 为什么不能指望客户端那个 index.cjs
//
// 实测（所有者本机，2026-10-16）：**国服客户端 `WorkBuddyAI` 的
// turing-sdk 目录里只有原生文件、没有 index.cjs**；而国际版客户端
// `WorkBuddyAIGloable` 里有。原实现把「index.cjs 存在」当判据 ⇒
// 国服客户端被判成「缺少组件」⇒ 国际版账号永远拿不到 X-Device-Token
// ⇒ **长期缺设备凭证被上游风控逐步标记**（见 device_token_server.rs 文件头）。
//
// 同理，若某天官方改了垫片的文件名/位置，我们的发现逻辑会再次静默失效 ——
// 而这里只有一行 require，自己带一份就彻底不依赖它了。
//
// # 为什么这是安全的（不是"重新逆向一遍"）
//
// 它**不实现任何协议逻辑**，只是把官方原生模块 require 出来并按原样转发。
// 参数（channelId / productName / productVersion）仍由 worker 逐字使用
// 官方 `cli/product.json` 的值 —— 与官方客户端取到的完全一致。
//
// ⚠ 原生模块与 DLL 是**腾讯的闭源二进制**，我们**无法自己生成**，仍必须
// 来自某个已安装的 WorkBuddy 客户端。本垫片不改变这一点，只是让
// 「客户端没带这个 1.2 KB 文件」不再导致整条链路失效。
'use strict';

const path = require('node:path');

// ★ 原生模块必须相对**客户端 SDK 目录**解析，不能相对 __dirname：
// 本垫片住在**我们自己的** assets/wb-device-token/ 里，而原生文件在客户端目录下。
// 第一版写成 path.join(__dirname, ...) ⇒ 直接找不到原生模块（实测报
// 「Cannot find module ...\assets\wb-device-token\build\Release\turing_sdk.node」）。
//
// 由 worker 在 require 本垫片**之前**设置该变量；未设置时退回 __dirname
//（覆盖"垫片被直接放进客户端目录"的情形，也让单独手工调试可用）。
const nativeDir = process.env.WB_TURING_NATIVE_DIR || __dirname;

let binding = null;
let loadError = null;

const supportedPlatform =
  process.platform === 'darwin' || process.platform === 'win32';

if (supportedPlatform) {
  try {
    binding = require(path.join(nativeDir, 'build', 'Release', 'turing_sdk.node'));
  } catch (error) {
    loadError = error;
    binding = null;
  }
} else {
  binding = null;
}

function isSupported() {
  return supportedPlatform && binding != null;
}

function getLoadError() {
  return loadError ? String(loadError.message || loadError) : null;
}

function requireBinding() {
  if (!binding) {
    throw new Error(
      `Turing SDK native binding unavailable: ${
        getLoadError() || `unsupported platform ${process.platform}`
      }`
    );
  }
  return binding;
}

module.exports = {
  isSupported,
  getLoadError,
  configure(channelId, productName, productVersion, keychainStorageEnabled) {
    return requireBinding().configure(
      channelId,
      productName,
      productVersion,
      keychainStorageEnabled
    );
  },
  fetchDeviceToken(options) {
    return requireBinding().fetchDeviceToken(options);
  },
};
