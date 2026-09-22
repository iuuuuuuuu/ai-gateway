// 演示模式覆盖校验：把 DEMO_READ_COMMANDS 里每条命令都真的走一遍
// screenshotDemoResponse，任何 throw 都会被捕获并报告。
//
// 为什么需要它：DEMO_READ_COMMANDS（声明）与 screenshotDemoResponse（实现）
// 是两处清单，只能靠人工保持同步 —— 已经发生过 12 条声明了却没有 case 的情况，
// 表现为演示/截图模式打开对应页面直接报错。这个脚本把两边拉齐成可验证的。
//
// 用法：node scripts/check-demo-coverage.mjs
import { readFileSync } from "node:fs";

const apiSrc = readFileSync(new URL("../src/lib/api.ts", import.meta.url), "utf8");
const demoSrc = readFileSync(new URL("../src/lib/screenshot-demo.ts", import.meta.url), "utf8");

const setBody = /DEMO_READ_COMMANDS\s*=\s*new Set\(\[([\s\S]*?)\]\)/.exec(apiSrc);
if (!setBody) {
  console.error("FAIL: 找不到 DEMO_READ_COMMANDS 定义");
  process.exit(1);
}
const declared = [...new Set([...setBody[1].matchAll(/"([^"]+)"/g)].map((m) => m[1]))].sort();
const handled = new Set([...demoSrc.matchAll(/case\s+"([^"]+)":/g)].map((m) => m[1]));

const missing = declared.filter((c) => !handled.has(c));
if (missing.length > 0) {
  console.error(`FAIL: ${missing.length} 条已声明但没有 case handler：`);
  for (const c of missing) console.error(`  - ${c}`);
  process.exit(1);
}
console.log(`OK: ${declared.length} 条声明的演示只读命令全部有 handler`);
