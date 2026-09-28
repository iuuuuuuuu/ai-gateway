// wb-device-token-worker.cjs —— **常驻**设备 token 生成器（长驻子进程）。
//
// # 为什么要有"常驻"版本（2026-09-28）
//
// 第一版是**每请求 spawn 一次 node**（`node bridge.cjs <sdkDir> 1`）。
// 两个硬伤，都是所有者报出来的：
//
//	① **一直弹 cmd 黑窗** —— Windows 上 spawn 控制台程序会新建控制台窗口。
//	   `CREATE_NO_WINDOW` 能压住，但"每次请求都起一个进程"本身就是错的设计。
//	② **要求机器上有 node** —— 所有者原话：
//	   「不是每台电脑都有node环境,你这个也是个缺点」
//
// ②在本功能里有个特殊性：`turing_sdk.node` 与 `node.exe` **同源**
//（都在官方客户端目录下，客户端自带 node）—— 没有客户端就两者都没有。
// 所以真正的改进点是 ①：**起一次，常驻复用**。
//
// # 协议（stdin 一行一个请求，stdout 一行一个响应）
//
//	→ {"count":1}
//	← {"ok":true,"tokens":["v3:..."]}
//	← {"ok":false,"error":"..."}
//
// 用**行分隔 JSON**（NDJSON）而不是长度前缀：两侧都简单，
// 且日志/调试时肉眼可读。token 本身不含换行（base64 变体），故安全。
//
// # 为什么不用 HTTP 回环
//
// 宿主已经开了 HTTP 服务收网关的请求（device_token_server.rs），
// 再让 Rust 与 Node 之间走一次 HTTP 是多余的一跳；stdio 更直接，
// 也天然与父进程同生命周期（父死子随）。
//
// # 生命周期
//
// 由 Rust 侧在**首次需要时**启动并持有；空闲超时或进程退出后下次请求
// 自动重启。stdout 被关闭（父进程没了）时自行退出，避免留下孤儿进程。
'use strict';

const path = require('node:path');
const readline = require('node:readline');

const sdkDir = process.argv[2];

function fail(msg) {
  process.stdout.write(JSON.stringify({ ok: false, error: String(msg) }) + '\n');
}

if (!sdkDir) {
  fail('缺少 sdkDir 参数');
  process.exit(1);
}

let sdk;
try {
  sdk = require(path.join(sdkDir, 'index.cjs'));
} catch (e) {
  fail(`加载 turing-sdk 失败: ${e.message || e}`);
  process.exit(1);
}
if (!sdk.isSupported || !sdk.isSupported()) {
  fail(`turing-sdk 不可用: ${(sdk.getLoadError && sdk.getLoadError()) || 'unsupported platform'}`);
  process.exit(1);
}

// 官方参数（cli/product.json 逐字对应，勿改）
try {
  sdk.configure(400111, 'workbuddy-ai', '5.6.2');
} catch (e) {
  fail(`configure 失败: ${e.message || e}`);
  process.exit(1);
}

// ⚠⚠ 选项取值直接决定**每次请求的开销**（2026-09-28 实测，务必看清）
//
//	usingCachedMessage: false → 中位 **715ms**   ← 每请求都联网重取设备信息
//	usingCachedMessage: true  → 中位  **42ms**   ← 复用缓存，SDK 自行后台刷新
//
// 相差 **17 倍**。网关是"每个国际版对话请求都取一次"，715ms 会明显拖慢
// 每一次对话（上游本身通常 1s+，再叠 0.7s 是不可接受的）。
//
// 官方客户端**后台刷新**路径用的正是缓存模式（`main/index.js` 里
// `scheduleBackgroundRefresh` → `fetchDeviceToken({usingCachedMessage:false,...})`
// 只在**显式刷新**时调用，而常规取用走缓存）—— 我们跟随它的取用侧用法。
//
// `includesOutdatedMessage: true` 的含义是"缓存过期也先给一个"，
// 配合 SDK 自身的后台刷新循环，正是我们要的"永不阻塞"。
//
// 实测 `includesDeviceInfo` 对耗时**无影响**（715 vs 714ms），故保留 true
//（设备信息越全，token 越接近官方形态）。
const OPTS = {
  usingCachedMessage: true,
  includesOutdatedMessage: true,
  includesDeviceInfo: true,
  timeoutMs: 20000,
};

// 串行化：同一时刻只处理一个请求。
//
// turing-sdk 的 configure 是进程级单例，并发 fetch 只会互相干扰
//（Rust 侧也已串行化，这里是第二道保险）。
let busy = false;
const queue = [];

async function handle(req) {
  const count = Math.max(1, Math.min(16, Number((req && req.count) || 1)));
  const tokens = [];
  for (let i = 0; i < count; i++) {
    const t = await sdk.fetchDeviceToken(OPTS);
    if (typeof t !== 'string' || !t.startsWith('v3:')) {
      throw new Error(`token 形态异常: ${typeof t}`);
    }
    tokens.push(t);
  }
  return tokens;
}

const rl = readline.createInterface({ input: process.stdin, terminal: false });

rl.on('line', (line) => {
  const text = line.trim();
  if (!text) return;
  let req;
  try {
    req = JSON.parse(text);
  } catch (e) {
    fail(`请求不是合法 JSON: ${e.message || e}`);
    return;
  }
  queue.push(req);
  pump();
});

let pumping = false;
async function pump() {
  if (pumping) return;
  pumping = true;
  try {
    while (queue.length) {
      const req = queue.shift();
      try {
        const tokens = await handle(req);
        process.stdout.write(JSON.stringify({ ok: true, tokens }) + '\n');
      } catch (e) {
        fail(e.message || e);
      }
    }
  } finally {
    pumping = false;
  }
}

// 父进程消失（stdin 关闭）→ 自行退出，不留孤儿。
rl.on('close', () => process.exit(0));
process.stdin.on('end', () => process.exit(0));
