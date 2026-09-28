// wb-device-token.cjs —— 为网关生成 WorkBuddy 设备 token（TuringShield）。
//
// # 为什么需要它
//
// 官方 WorkBuddy 客户端每个请求都带 `X-Device-Token`（`v3:` 形态，约 1030 字符），
// 而网关此前**完全不带**。实测（2026-09-28）：
//
//	- token 由官方自带的 turing-sdk 生成，参数来自 cli/product.json：
//	    channelId=400111、sdkVariant="overseas"（isOversea=true）、
//	    productName="workbuddy-ai"、productVersion="5.6.2"
//	- 生成耗时 39~51ms（首次 122ms），**每次调用都不同**（按请求签发）
//	- 注入它**救不活**已被上游标记失效的账号（那是服务端绑定状态），
//	  但也不会影响可用账号 —— 价值在于让网关请求与官方客户端**同形态**，
//	  避免长期缺设备凭证被风控逐步标记。
//
// # 用法（被宿主以子进程方式调用）
//
//	<node> wb-device-token.cjs <sdkDir> <count>
//
// 输出 JSON 到 stdout：
//	{"ok":true,"tokens":["v3:...","v3:..."]}
//	{"ok":false,"error":"..."}
//
// # 安全说明
//
// token 只经由 stdout 返回给**本机宿主进程**，不落盘、不打日志。
// 本脚本不读取任何账号凭证 —— 它只做"设备身份"这一件事。
'use strict';

const path = require('node:path');

const sdkDir = process.argv[2];
const count = Math.max(1, Math.min(16, Number(process.argv[3] || 1)));

function fail(msg) {
  process.stdout.write(JSON.stringify({ ok: false, error: String(msg) }) + '\n');
  process.exit(0);
}

if (!sdkDir) fail('缺少 sdkDir 参数');

let sdk;
try {
  sdk = require(path.join(sdkDir, 'index.cjs'));
} catch (e) {
  fail(`加载 turing-sdk 失败: ${e.message || e}`);
}

if (!sdk.isSupported || !sdk.isSupported()) {
  fail(`turing-sdk 不可用: ${(sdk.getLoadError && sdk.getLoadError()) || 'unsupported platform'}`);
}

// 官方参数（cli/product.json 逐字对应，勿改）
const CHANNEL_ID = 400111;
const PRODUCT_NAME = 'workbuddy-ai';
const PRODUCT_VERSION = '5.6.2';

try {
  sdk.configure(CHANNEL_ID, PRODUCT_NAME, PRODUCT_VERSION);
} catch (e) {
  fail(`configure 失败: ${e.message || e}`);
}

const OPTS = {
  usingCachedMessage: false,
  includesOutdatedMessage: true,
  includesDeviceInfo: true,
  timeoutMs: 20000,
};

(async () => {
  const tokens = [];
  for (let i = 0; i < count; i++) {
    try {
      const t = await sdk.fetchDeviceToken(OPTS);
      if (typeof t === 'string' && t.startsWith('v3:')) {
        tokens.push(t);
      } else {
        fail(`token 形态异常: ${typeof t}`);
      }
    } catch (e) {
      fail(`fetchDeviceToken 失败: ${e.message || e}`);
    }
  }
  process.stdout.write(JSON.stringify({ ok: true, tokens }) + '\n');
})();
