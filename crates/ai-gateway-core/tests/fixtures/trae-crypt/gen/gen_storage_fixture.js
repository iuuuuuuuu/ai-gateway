'use strict';
// ---------------------------------------------------------------------------
// storage 级跨实现夹具生成器
//
// 与 gen_vectors.js 的分工：
//   gen_vectors.js         —— 容器级（decryptTcBuffer）正向/负向向量
//   gen_storage_fixture.js —— storage 级（decryptAllEncryptedValues / decryptAuthData）
//
// ★ 本脚本**真的把夹具写成磁盘上的 `<dir>/User/globalStorage/storage.json`**，
//   然后**真的调用参考实现的 decryptAllEncryptedValues(dir) 与 decryptAuthData(dir)**，
//   而不是在内存里复述它的循环。这样 Rust 侧的解密结果是与「参考实现实际跑出来的
//   输出」对拍，而不是与「我理解中的参考实现」对拍。
//
// 夹具全部是**合成值**（无任何真实账号数据）；密文由 gen_vectors.js 同源的
// 加密逻辑用「从参考实现源码读出的盐」现场生成。
//
// ★★ 脚本内置「长数字标识守卫」（见本文件末尾）：夹具里一旦出现**未声明的**
//    15 位以上数字串，立即抛错中止，不写任何文件。历史缺陷：`icube_gtm.users`
//    曾照抄真实账号 uid，导致 `_syntheticOnly: true` 自述与内容不符。
//
// 复现：node test-bin/verify-trae/gen_storage_fixture.js
// ---------------------------------------------------------------------------

const crypto = require('crypto');
const fs = require('fs');
const path = require('path');

const REF = path.resolve(__dirname, '..', 'recon', 'trae-wangqi233', 'src', 'trae-decrypt.js');
const refSrc = fs.readFileSync(REF, 'utf8');
const ref = require(REF);

const APP_DATA = path.resolve(__dirname, 'fixture-appdata');
const STORAGE_PATH = path.join(APP_DATA, 'User', 'globalStorage', 'storage.json');
const OUT = path.resolve(__dirname, 'storage_fixture.json');

const HEADER_AES = Buffer.from([0x74, 0x63, 0x05, 0x10, 0x00, 0x00]);
const HEADER_PRIVATE = Buffer.from([18, 57, 32, 32, 2, 3]);

function sha512(b) {
  return crypto.createHash('sha512').update(b).digest();
}
function xorBuf(a, b) {
  const r = Buffer.alloc(64);
  for (let i = 0; i < 64; i++) r[i] = a[i] ^ b[i];
  return r;
}
function parseSalt(name) {
  const re = new RegExp('const ' + name + '\\s*=\\s*[\\s\\S]*?Uint8Array\\.from\\(\\[([^\\]]*)\\]\\)');
  const m = refSrc.match(re);
  if (!m) throw new Error('解析失败：找不到 ' + name);
  const nums = m[1].split(',').map((s) => s.trim()).filter((s) => s.length > 0).map((s) => parseInt(s, 10));
  if (nums.length !== 64) throw new Error(name + ' 元素个数不是 64');
  return Buffer.from(nums);
}
const SALTS = { A: parseSalt('SALT_A'), B: parseSalt('SALT_B'), C: parseSalt('SALT_C'), D: parseSalt('SALT_D') };

function kdfIndependent(randomBytes, encType) {
  const salt = encType === 'AES_PRIVATE' ? xorBuf(SALTS.C, SALTS.D) : xorBuf(SALTS.A, SALTS.B);
  return sha512(Buffer.concat([sha512(randomBytes), salt]));
}
// 与参考实现导出的 deriveKeyAndIV 对拍（防止本脚本自己的 KDF 写错）
for (const encType of ['AES', 'AES_PRIVATE']) {
  const rb = crypto.createHash('sha256').update('storage-kdf-check:' + encType).digest().subarray(0, 32);
  const mine = kdfIndependent(rb, encType);
  const theirs = ref.deriveKeyAndIV(rb, encType);
  if (!mine.subarray(0, 32).equals(Buffer.concat([Buffer.from(theirs.aesKey), Buffer.from(theirs.iv)]))) {
    throw new Error('KDF 对拍失败: ' + encType);
  }
}

function rbFor(tag) {
  return crypto.createHash('sha256').update('storage-random-bytes:' + tag).digest().subarray(0, 32);
}
function encryptContainer(plainBuf, encType, randomBytes) {
  const kdf = ref.deriveKeyAndIV(randomBytes, encType);
  const body = Buffer.concat([sha512(plainBuf), plainBuf]);
  const cipher = crypto.createCipheriv('aes-128-cbc', Buffer.from(kdf.aesKey), Buffer.from(kdf.iv));
  const ct = Buffer.concat([cipher.update(body), cipher.final()]);
  return Buffer.concat([encType === 'AES_PRIVATE' ? HEADER_PRIVATE : HEADER_AES, randomBytes, ct]);
}

// --- 构造夹具 ---------------------------------------------------------------
const fixture = {};
const expect = [];
// expectedValue = **Rust 侧必须产出的值**（独立于 JS 是否产出，因此能把两者的
//                 分歧变成可断言的机器证据，而不是一句「我们不一样」）
// encType       = 该条密文用的头（非密文为 null），用于报告里区分两种头
function add(key, rawValue, kind, expectedValue, encType, note) {
  fixture[key] = rawValue;
  expect.push({ key: key, kind: kind, expectedValue: expectedValue, encType: encType, note: note });
}

// ★★ 合成账号标识 —— **唯一允许出现在夹具里的 15+ 位数字串**。
//    真实 gtm uid / dc uid 不得进入仓库；末尾的守卫会在出现未声明数字串时中止。
const SYNTH_GTM_UID = '1883919207380040'; // 与 trae_discover.rs / trae_account.rs 既有合成值一致
const SYNTH_DC_UID = '199439841787403';   // 与 trae_discover.rs 既有 dc uid 合成值一致

const AUTH_KEY = 'iCubeAuthInfo://icube.cloudide';
const AUTH_OBJ = { uid: 'u-synthetic-1001', plan: 'pro', exp: 1735689600 };

add('iCubeServerData://icube.cloudide', JSON.stringify({ tier: 'pro', seats: 3 }),
  'plaintext_kept', JSON.stringify({ tier: 'pro', seats: 3 }), null,
  '明文 JSON（以 { 开头）：两侧都不解密，原样保留');
add('plaintext.quoted', JSON.stringify('just-a-string'),
  'plaintext_kept', JSON.stringify('just-a-string'), null,
  '以双引号开头的明文：两侧都不解密');
add(AUTH_KEY, encryptContainer(Buffer.from(JSON.stringify(AUTH_OBJ), 'utf8'), 'AES', rbFor('auth')).toString('base64'),
  'decrypted_json', AUTH_OBJ, 'AES',
  'AES 头 + 合法 JSON 对象：JS results[k]=JSON.parse，Rust 也换成结构化值（用于 decryptAuthData 真调用）');
add(`iCubeAuthInfo://icube-dc:${SYNTH_DC_UID}`,
  encryptContainer(Buffer.from('{"uid":"u-synthetic-2002","plan":"solo"}', 'utf8'), 'AES_PRIVATE', rbFor('dc')).toString('base64'),
  'decrypted_json', { uid: 'u-synthetic-2002', plan: 'solo' }, 'AES_PRIVATE',
  '★★ AES_PRIVATE 头 + 合法 JSON：Rust 必须解出；参考实现的 decryptAllEncryptedValues 因 isTcEncrypted 只认 tc 头会**静默跳过**它（实测分歧）');
add('iCubeAuthInfo://usertag',
  encryptContainer(Buffer.from('"synthetic-usertag"', 'utf8'), 'AES', rbFor('usertag')).toString('base64'),
  'decrypted_json', 'synthetic-usertag', 'AES',
  'AES 头 + JSON 字符串标量');
add('nonjson.encrypted',
  encryptContainer(Buffer.from('not json at all ~~~', 'utf8'), 'AES', rbFor('nonjson')).toString('base64'),
  'rust_keeps_plain_string', 'not json at all ~~~', 'AES',
  '★ 刻意分歧：JS 塞 {_decryptError} 占位对象，Rust 保留解出的明文串');
{
  const buf = encryptContainer(Buffer.from('{"x":1}', 'utf8'), 'AES', rbFor('corrupt'));
  const bad = Buffer.from(buf);
  bad[bad.length - 1] ^= 0x01;
  add('corrupt.encrypted', bad.toString('base64'),
    'rust_keeps_original', bad.toString('base64'), 'AES',
    '★ 刻意分歧：合法 tc 头但密文被篡改 ⇒ JS 塞占位对象，Rust 保留原值 + stats.failed++');
}
add('telemetry.sqmId', 'short', 'skipped_kept', 'short', null, '长度 5 < 10：两侧都跳过');
add('__import__vscode__extensions', 'this-is-not-base64-container', 'skipped_kept', 'this-is-not-base64-container', null,
  '长度 >= 10 但非 base64 容器：两侧都跳过');
add('telemetry.devDeviceId', 'a1b2c3d4e5f6a7b8c9d0e1f2', 'skipped_kept', 'a1b2c3d4e5f6a7b8c9d0e1f2', null,
  'hex 串（24 字符），头两字节不是 tc：两侧都跳过');
const GTM_OBJ = { users: { [SYNTH_GTM_UID]: { gtm101_idea: { showCount: 1, closed: true } } } };
add('icube_gtm', GTM_OBJ, 'non_string_kept', GTM_OBJ, null,
  '★ 嵌套对象（形态照抄本机真实结构，内层 uid 已换合成值）：两侧都跳过并原样保留');
add('windowControlHeight', 30, 'non_string_kept', 30, null, '数字：原样保留');
add('windowSplash', true, 'non_string_kept', true, null, '布尔：原样保留');

// --- ★★ 合成值守卫：未声明的长数字标识一律中止（在任何落盘之前） -------------
// 真实账号标识只可能出现在**键名**与**非密文的结构化值**里（历史缺陷正是
// `icube_gtm.users` 照抄了真实 uid）。密文是 base64，长度 >= 60 的字符串值视为
// 不透明密文跳过扫描，避免把密文里偶然出现的数字串误判成标识。
{
  const sources = [];
  for (const k of Object.keys(fixture)) {
    sources.push(['key', k]);
    const v = fixture[k];
    if (typeof v === 'string') {
      if (v.length < 60) sources.push(['value', v]);
    } else {
      sources.push(['value', JSON.stringify(v)]);
    }
  }
  const allowed = new Set([SYNTH_GTM_UID, SYNTH_DC_UID]);
  const offenders = [];
  for (const [where, text] of sources) {
    for (const run of text.match(/\d{15,}/g) || []) {
      if (!allowed.has(run)) offenders.push(where + ':' + run);
    }
  }
  if (offenders.length > 0) {
    throw new Error('★ 夹具里出现未声明的长数字标识（疑似真实账号数据，已中止且未落盘）: ' + JSON.stringify(offenders));
  }
  console.log('SYNTH_GUARD        = OK（长数字标识仅 ' + [...allowed].join(' / ') + '）');
}

// --- 真的落盘，再真的调用参考实现 -------------------------------------------
fs.mkdirSync(path.dirname(STORAGE_PATH), { recursive: true });
fs.writeFileSync(STORAGE_PATH, JSON.stringify(fixture, null, 2).replace(/\n/g, '\r\n'), 'utf8');

const jsResults = ref.decryptAllEncryptedValues(APP_DATA); // ★ 真调用
let jsAuthData = null;
let jsAuthError = null;
try {
  jsAuthData = ref.decryptAuthData(APP_DATA); // ★ 真调用
} catch (e) {
  jsAuthError = e.message;
}

// ★ 逐条用**单值 API**（decryptStorageValue，不做 tc 头初筛）跑一遍：
//   凡是单值 API 能解开、但 bulk API 没出现在 results 里的键 ⇒ 证明「容器本身合法，
//   是 bulk 的 isTcEncrypted 初筛把它漏掉了」，而不是「密文坏了」。
const jsSingleValue = {};
for (const [k, v] of Object.entries(fixture)) {
  if (typeof v !== 'string') continue;
  try {
    jsSingleValue[k] = { ok: true, value: ref.decryptStorageValue(v) };
  } catch (e) {
    jsSingleValue[k] = { ok: false, error: e.message };
  }
}
const missedByBulkButDecryptable = [];
for (const [k, r] of Object.entries(jsSingleValue)) {
  if (r.ok && !(k in jsResults)) missedByBulkButDecryptable.push(k);
}

// --- stats 独立预言机 --------------------------------------------------------
// 按 trae_crypt.rs:384-443 记录的规则**独立**重算一遍 scanned/encrypted/decrypted/failed，
// 供 Rust 侧断言。规则（与参考实现 decryptAllEncryptedValues 的差异已记录在
// trae_crypt.rs 的文档注释里）：
//   scanned    = 字符串值个数
//   encrypted  = 通过「非明文快路径」+「长度 >= 10」+「头是 tc 或 AES_PRIVATE」的个数
//   decrypted  = 其中 decrypt_tc_buffer 成功的个数
//   failed     = 其中失败的个数
const HEADER_AES_HEX = HEADER_AES.toString('hex');
const HEADER_PRIVATE_HEX = HEADER_PRIVATE.toString('hex');
function rustWouldTreatAsEncrypted(v) {
  const t = v.replace(/^\s+/, '');
  if (t.startsWith('{') || t.startsWith('"')) return false; // 明文快路径
  if (v.trim().length < 10) return false;
  let raw;
  try {
    raw = Buffer.from(v.trim().replace(/\s+/g, ''), 'base64');
  } catch (e) {
    return false;
  }
  if (raw.length < 6) return false;
  const h = raw.subarray(0, 6).toString('hex');
  return h === HEADER_AES_HEX || h === HEADER_PRIVATE_HEX;
}
const expectStats = { scanned: 0, encrypted: 0, decrypted: 0, failed: 0 };
const perKeyDecision = [];
for (const [k, v] of Object.entries(fixture)) {
  if (typeof v !== 'string') {
    perKeyDecision.push({ key: k, decision: 'non_string' });
    continue;
  }
  expectStats.scanned += 1;
  if (!rustWouldTreatAsEncrypted(v)) {
    const t = v.replace(/^\s+/, '');
    const why = (t.startsWith('{') || t.startsWith('"')) ? 'plaintext_fastpath'
      : (v.trim().length < 10 ? 'too_short' : 'not_encrypted_header');
    perKeyDecision.push({ key: k, decision: why });
    continue;
  }
  expectStats.encrypted += 1;
  const r = jsSingleValue[k];
  if (r && r.ok) {
    expectStats.decrypted += 1;
    perKeyDecision.push({ key: k, decision: 'decrypted' });
  } else {
    expectStats.failed += 1;
    perKeyDecision.push({ key: k, decision: 'failed' });
  }
}

const out = {
  _generatedBy: 'test-bin/verify-trae/gen_storage_fixture.js',
  _referenceImplementation: 'test-bin/recon/trae-wangqi233/src/trae-decrypt.js',
  _referenceCall: 'ref.decryptAllEncryptedValues(<fixture-appdata>) 与 ref.decryptAuthData(<fixture-appdata>) —— 真落盘 + 真调用，非内存复述',
  _nodeVersion: process.version,
  _fixturePath: 'test-bin/verify-trae/fixture-appdata/User/globalStorage/storage.json',
  _syntheticOnly: true,
  _syntheticIds: { gtmUid: SYNTH_GTM_UID, dcUid: SYNTH_DC_UID },
  _syntheticGuard: 'gen_storage_fixture.js 内置守卫：夹具里出现未声明的 15+ 位数字串即抛错中止（不落盘）',
  _jsStorageResults: jsResults,
  _jsAuthData: jsAuthData,
  _jsAuthError: jsAuthError,
  _jsSingleValue: jsSingleValue,
  _missedByBulkButDecryptable: missedByBulkButDecryptable,
  _expectStats: expectStats,
  _perKeyDecision: perKeyDecision,
  _expect: expect,
  fixture: fixture,
};

fs.writeFileSync(OUT, JSON.stringify(out, null, 2).replace(/\n/g, '\r\n'), 'utf8');

console.log('STORAGE_FIXTURE    = ' + STORAGE_PATH);
console.log('OUT                = ' + OUT);
console.log('NODE               = ' + process.version);
console.log('KEYS               = ' + Object.keys(fixture).length);
console.log('JS_STORAGE_KEYS    = ' + Object.keys(jsResults).length + ' -> ' + JSON.stringify(Object.keys(jsResults)));
for (const [k, v] of Object.entries(jsResults)) {
  console.log('  js[' + k + '] = ' + JSON.stringify(v).slice(0, 120));
}
console.log('JS_AUTH_DATA       = ' + JSON.stringify(jsAuthData));
console.log('JS_AUTH_ERROR      = ' + jsAuthError);
console.log('JS_SINGLE_VALUE_OK = ' + JSON.stringify(Object.entries(jsSingleValue).filter(([, r]) => r.ok).map(([k]) => k)));
console.log('★ MISSED_BY_BULK_BUT_DECRYPTABLE = ' + JSON.stringify(missedByBulkButDecryptable));
console.log('EXPECT_STATS       = ' + JSON.stringify(expectStats));
for (const d of perKeyDecision) console.log('  ' + d.decision.padEnd(22) + ' ' + d.key);
console.log('EXPECT_KINDS       = ' + JSON.stringify(expect.reduce((a, e) => { a[e.kind] = (a[e.kind] || 0) + 1; return a; }, {})));
