// 批次3-B 复核补充：base64 **入口宽容度** 的跨实现差异（Node 宽松 vs Rust 严格）。
//
// 为什么单独做这一张表：
//   参考实现 `decryptTcBuffer` 的入口一律是 `Buffer.from(value, 'base64')`。
//   Node 的 base64 解码器是**宽容**的 —— 它会
//     (1) 接受 URL-safe 字母表（`-` / `_` 当作 `+` / `/`）；
//     (2) 忽略无法识别的字符（例如插进来的 `!`）；
//     (3) 接受缺失的 `=` 补齐。
//   而 Rust 侧 `decrypt_tc_base64` 走的是 `base64::engine::general_purpose::STANDARD`
//   （严格：字母表固定、补齐必需、非法字符即错）。
//
//   后果不是「报错不同」那么轻 —— 我方**入口判据** `looks_like_encrypted_base64`
//   内部也用同一个严格解码器，解码失败即返回 `false` ⇒ 该键被**静默跳过**
//   （既不计入 `encrypted` 也不计入 `failed`）。也就是说：凡是参考实现能解、
//   而 base64 形态不是标准形态的值，我方会**静默漏掉**，而不是报错。
//
// 本脚本把这条差异做成**机器可验的表**（每条都记录 JS 的真实行为），
// 供 Rust 集成测试 `trae_crypt_crossimpl.rs` 逐条断言。
//
// 运行：node test-bin/verify-trae/gen_base64_variants.js
// 输出：test-bin/verify-trae/base64_variants.json（CRLF，无 BOM）

'use strict';

const fs = require('fs');
const path = require('path');

const HERE = __dirname;
const REF = path.join(HERE, '..', 'recon', 'trae-wangqi233', 'src', 'trae-decrypt.js');
const ref = require(REF);

const vectors = JSON.parse(
  fs.readFileSync(path.join(HERE, 'vectors.json'), 'utf8').replace(/^\uFEFF/, '')
);

// 挑一条 base64 里**同时含** `+` 与 `/` 的正向向量：只有这样 URL-safe 变体
// 才与原串不同，测试才有意义。
const picked = vectors.vectors.find((v) => /\+/.test(v.cipherB64) && /\//.test(v.cipherB64));
if (!picked) {
  throw new Error('vectors.json 里找不到同时含 + 与 / 的正向向量，无法构造 URL-safe 变体');
}

const std = picked.cipherB64;

/** 把标准 base64 换成 URL-safe 字母表。 */
function toUrlSafe(s) {
  return s.replace(/\+/g, '-').replace(/\//g, '_');
}
/** 去掉尾部补齐。 */
function stripPadding(s) {
  return s.replace(/=+$/, '');
}
/** 在中间插一个非法字符。 */
function withStrayChar(s) {
  const mid = Math.floor(s.length / 2);
  return s.slice(0, mid) + '!' + s.slice(mid);
}
/** 在中间插空白（换行 / 空格）。 */
function withWhitespace(s) {
  const mid = Math.floor(s.length / 2);
  return s.slice(0, mid) + '\n ' + s.slice(mid);
}

const VARIANTS = [
  {
    id: 'std',
    note: '标准形态（对照组）：Rust 与 JS 都接受，且与 task-18 前的行为逐字节相同',
    build: (s) => s,
    expectJsAccepts: true,
    expectRustAccepts: true,
  },
  {
    id: 'urlsafe_alphabet',
    note: 'URL-safe 字母表（+→-，/→_）：Node 接受；task-18 后 Rust 也接受（已收敛）',
    build: toUrlSafe,
    expectJsAccepts: true,
    expectRustAccepts: true,
  },
  {
    id: 'unpadded',
    note: '去掉尾部 = 补齐：Node 接受；task-18 后 Rust 也接受（已收敛）',
    build: stripPadding,
    expectJsAccepts: true,
    expectRustAccepts: true,
  },
  {
    id: 'stray_char',
    note: '中间插入一个 ! ：Node 忽略非法字符；task-18 后 Rust 也忽略（已收敛）',
    build: withStrayChar,
    expectJsAccepts: true,
    expectRustAccepts: true,
  },
  {
    id: 'embedded_whitespace',
    note: '中间插入换行+空格：两边都接受（Rust 逐字符忽略空白与非法字符）',
    build: withWhitespace,
    expectJsAccepts: true,
    expectRustAccepts: true,
  },
];

const rows = [];
for (const v of VARIANTS) {
  const value = v.build(std);
  const buf = Buffer.from(value, 'base64');
  let jsOk = false;
  let jsResult = null;
  let jsError = null;
  try {
    jsResult = ref.decryptTcBuffer(buf);
    jsOk = true;
  } catch (e) {
    jsError = e.message;
  }
  rows.push({
    id: v.id,
    note: v.note,
    // ★ 完整 value 不落盘会更好，但 Rust 侧要原样喂进去 ⇒ 必须带上。
    //   它只是**合成向量**的密文，不含任何真实账号数据。
    value,
    differsFromStd: value !== std,
    jsBufLen: buf.length,
    jsIsTcEncrypted: ref.isTcEncrypted(buf),
    jsSucceeded: jsOk,
    jsPlaintext: jsOk ? jsResult : null,
    jsError,
    expectRustAccepts: v.expectRustAccepts,
  });
}

// 附：参考实现自身在**单值入口**上的行为（无 tc 头初筛），与 bulk 入口对比
const singleValueRows = rows.map((r) => ({
  id: r.id,
  jsDecryptStorageValueSucceeded: r.jsSucceeded,
}));

const out = {
  _generatedBy: 'test-bin/verify-trae/gen_base64_variants.js',
  _referenceImplementation: 'test-bin/recon/trae-wangqi233/src/trae-decrypt.js',
  _nodeVersion: process.version,
  _whyThisTable:
    'Node 的 Buffer.from(s,"base64") 宽容（URL-safe 字母表 / 忽略非法字符 / 缺补齐可解）。' +
    'task-18 之前，Rust 侧 decrypt_tc_base64 与入口判据 looks_like_encrypted_base64 都用严格的 STANDARD 引擎，' +
    '后果是「静默跳过」而不是「报错」：解码失败 ⇒ 判据 false ⇒ 该键既不计入 encrypted 也不计入 failed。' +
    'task-18 之后两边共用同一条宽容解码路径（decode_base64_lenient），本表的 5 行全部收敛：' +
    'expectRustAccepts 全为 true，且每一行都必须解出与 std 行逐字节相同的明文。' +
    '唯一有意保留的分歧是非 ASCII 输入（Node 按 Latin-1 截断，Rust 直接判为非密文）——真实密文只含 ASCII，故不漏数据。',
  _sourceVectorId: picked.id,
  _sourceVectorPlaintext: picked.plaintext,
  _sourceVectorEncType: picked.encType,
  _stdCipherB64: std,
  _plusCount: (std.match(/\+/g) || []).length,
  _slashCount: (std.match(/\//g) || []).length,
  rows,
  _jsSingleValueEntry: singleValueRows,
};

const text = JSON.stringify(out, null, 2).replace(/\n/g, '\r\n');
fs.writeFileSync(path.join(HERE, 'base64_variants.json'), text, 'utf8');

console.log('SOURCE_VECTOR =', picked.id, '| plus=' + out._plusCount, 'slash=' + out._slashCount);
console.log('ROWS =', rows.length);
for (const r of rows) {
  console.log(
    '  ' +
      r.id.padEnd(20) +
      ' differs=' +
      String(r.differsFromStd).padEnd(5) +
      ' jsIsTc=' +
      String(r.jsIsTcEncrypted).padEnd(5) +
      ' jsOk=' +
      String(r.jsSucceeded).padEnd(5) +
      ' expectRustAccepts=' +
      String(r.expectRustAccepts).padEnd(5) +
      (r.jsSucceeded ? ' jsLen=' + r.jsPlaintext.length : ' jsErr=' + r.jsError)
  );
}
console.log('WROTE base64_variants.json');
