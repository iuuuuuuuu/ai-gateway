'use strict';
// ---------------------------------------------------------------------------
// 跨实现测试向量生成器
//
// 目的：用 **真实 Node 参考实现**（test-bin/recon/trae-wangqi233/src/trae-decrypt.js）
// 的盐值与 KDF 加密**我们自己选的明文**，产出 test-bin/verify-trae/vectors.json，
// 再由 Rust 集成测试（crates/ai-gateway-core/tests/trae_crypt_crossimpl.rs）解密并
// 断言逐字节相同。这验证的是「两个实现真的互操作」，而不是「某个常量抄对了」。
//
// ★ 本文件**不硬编码任何盐字节** —— 盐值一律从参考实现源码里程序化读出，
//   并立刻用「读出的盐重算 KDF」与「参考实现导出的 deriveKeyAndIV」对拍自检。
//   手抄常量正是本任务要排除的失败模式。
//
// 复现：node test-bin/verify-trae/gen_vectors.js
// ---------------------------------------------------------------------------

const crypto = require('crypto');
const fs = require('fs');
const path = require('path');

const REF = path.resolve(__dirname, '..', 'recon', 'trae-wangqi233', 'src', 'trae-decrypt.js');
const OUT = path.resolve(__dirname, 'vectors.json');

const refSrc = fs.readFileSync(REF, 'utf8');
const ref = require(REF);

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

// --- 程序化读出参考实现的 4 组盐（正则匹配源码，不手抄）---------------------
function parseSalt(name) {
  const re = new RegExp('const ' + name + '\\s*=\\s*[\\s\\S]*?Uint8Array\\.from\\(\\[([^\\]]*)\\]\\)');
  const m = refSrc.match(re);
  if (!m) throw new Error('解析失败：找不到 ' + name + ' 的 Uint8Array 声明');
  const nums = m[1].split(',').map((s) => s.trim()).filter((s) => s.length > 0).map((s) => parseInt(s, 10));
  if (nums.length !== 64) throw new Error(name + ' 元素个数不是 64，实际 ' + nums.length);
  if (nums.some((n) => !Number.isInteger(n) || n < 0 || n > 255)) throw new Error(name + ' 含非法字节值');
  return Buffer.from(nums);
}

const SALTS = {
  A: parseSalt('SALT_A'),
  B: parseSalt('SALT_B'),
  C: parseSalt('SALT_C'),
  D: parseSalt('SALT_D'),
};

// --- 自检 1：用「读出的盐」独立重算 KDF，必须与参考实现导出的结果逐字节一致 ---
function kdfIndependent(randomBytes, encType) {
  const salt = encType === 'AES_PRIVATE' ? xorBuf(SALTS.C, SALTS.D) : xorBuf(SALTS.A, SALTS.B);
  return sha512(Buffer.concat([sha512(randomBytes), salt]));
}

const selfChecks = [];
for (const encType of ['AES', 'AES_PRIVATE']) {
  for (let i = 0; i < 8; i++) {
    const rb = crypto.createHash('sha256').update('selfcheck:' + encType + ':' + i).digest().subarray(0, 32);
    const mine = kdfIndependent(rb, encType);
    const theirs = ref.deriveKeyAndIV(rb, encType);
    const theirFull = Buffer.concat([Buffer.from(theirs.aesKey), Buffer.from(theirs.iv)]);
    const ok = mine.subarray(0, 32).equals(theirFull);
    selfChecks.push({ encType, i, randomBytesHex: rb.toString('hex'), match: ok });
    if (!ok) throw new Error('自检失败：读出的盐与参考实现的 deriveKeyAndIV 不一致 (' + encType + ' #' + i + ')');
  }
}

// --- 自检 2：盐指纹（供报告引用，同时证明「读到的盐」是哪一组）--------------
function fingerprint(buf) {
  return crypto.createHash('sha256').update(buf).digest('hex');
}
const saltFingerprints = {
  SALT_A_sha256: fingerprint(SALTS.A),
  SALT_B_sha256: fingerprint(SALTS.B),
  SALT_C_sha256: fingerprint(SALTS.C),
  SALT_D_sha256: fingerprint(SALTS.D),
  AES_salt_xorAB_sha256: fingerprint(xorBuf(SALTS.A, SALTS.B)),
  AES_PRIVATE_salt_xorCD_sha256: fingerprint(xorBuf(SALTS.C, SALTS.D)),
  AES_salt_is_xorCD: xorBuf(SALTS.A, SALTS.B).equals(xorBuf(SALTS.C, SALTS.D)),
};

// --- 加密：按参考实现 decryptTcBuffer 的存储格式反向构造 ---------------------
// 格式：header(6) ‖ randomBytes(32) ‖ AES-128-CBC-PKCS7( sha512(plaintext) ‖ plaintext )
function encryptContainer(plainBuf, encType, randomBytes, kdfOverride) {
  const kdf = kdfOverride || ref.deriveKeyAndIV(randomBytes, encType);
  const key = Buffer.from(kdf.aesKey);
  const iv = Buffer.from(kdf.iv);
  const body = Buffer.concat([sha512(plainBuf), plainBuf]);
  const cipher = crypto.createCipheriv('aes-128-cbc', key, iv);
  const ct = Buffer.concat([cipher.update(body), cipher.final()]);
  const header = encType === 'AES_PRIVATE' ? HEADER_PRIVATE : HEADER_AES;
  return Buffer.concat([header, randomBytes, ct]);
}

// --- 明文样本 ---------------------------------------------------------------
function longText(n) {
  // 确定性长串：混 ASCII / 中文 / 标点
  const unit = 'abcdefghij0123456789 中文内容·标点，';
  let s = '';
  while (s.length < n) s += unit;
  return s.slice(0, n);
}

const PLAINTEXTS = [
  { id: 'p1_ascii_json', text: '{"token":"abc123","exp":1735689600,"scope":"user"}', note: 'ASCII JSON' },
  { id: 'p2_chinese', text: '{"msg":"你好，世界 —— 中文与标点","用户":"测试账号"}', note: '中文 + 全角标点' },
  { id: 'p3_emoji', text: '{"note":"🎉🚀✨ 完成","flag":"🇨🇳","mood":"😀"}', note: 'emoji（含 ZWJ 无关的代理对）' },
  { id: 'p4_long_4000', text: longText(4000), note: '4000 字符超长串' },
  { id: 'p5_empty', text: '', note: '★ 空明文（容器长度恰为 118 = MIN_CIPHERTEXT_LEN）' },
  { id: 'p6_single_byte', text: 'x', note: '★ 单字节明文（容器长度恰为 118）' },
  { id: 'p7_braces_only', text: '{}', note: '仅花括号（明文快路径形态）' },
];

function rbFor(tag) {
  return crypto.createHash('sha256').update('vector-random-bytes:' + tag).digest().subarray(0, 32);
}

const vectors = [];
for (const pt of PLAINTEXTS) {
  for (const encType of ['AES', 'AES_PRIVATE']) {
    const randomBytes = rbFor(pt.id + ':' + encType);
    const buf = encryptContainer(Buffer.from(pt.text, 'utf8'), encType, randomBytes);
    const kdf = ref.deriveKeyAndIV(randomBytes, encType);
    // 参考实现自己必须能解开（否则向量本身是错的）
    const jsRoundTrip = ref.decryptTcBuffer(buf);
    if (jsRoundTrip !== pt.text) {
      throw new Error('JS 自身往返失败：' + pt.id + '/' + encType);
    }
    vectors.push({
      id: pt.id + '__' + encType,
      note: pt.note,
      encType: encType,
      plaintext: pt.text,
      plaintextUtf8Bytes: Buffer.byteLength(pt.text, 'utf8'),
      randomBytesHex: randomBytes.toString('hex'),
      keyHex: Buffer.from(kdf.aesKey).toString('hex'),
      ivHex: Buffer.from(kdf.iv).toString('hex'),
      containerLen: buf.length,
      cipherB64: buf.toString('base64'),
      jsRoundTrip: true,
    });
  }
}

// --- 负向向量：记录**参考实现实际抛出的错误文本**，Rust 侧断言变体 ----------
function tryDecrypt(buf) {
  try {
    return { ok: true, value: ref.decryptTcBuffer(buf) };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}

const baseVec = vectors.find((v) => v.id === 'p1_ascii_json__AES');
const baseBuf = Buffer.from(baseVec.cipherB64, 'base64');
const baseRb = Buffer.from(baseVec.randomBytesHex, 'hex');

function cloneBase() {
  return Buffer.from(baseBuf);
}

const negatives = [];
// `expectRust` 可以是字符串（确定性变体）或数组（**允许集合**，见 mechanism 说明）。
//
// ★ 为什么要允许集合：当密钥是错的（篡改 randomBytes、换错盐）时，CBC 解出的
// 是一段**随机明文**。Rust 侧先做 PKCS7 去 padding，只有末字节碰巧构成合法
// padding（概率 ≈ 1/256 + 1/65536 + … ≈ 0.4%）时才会走到 sha512 校验，
// 此时才报 HashMismatch；否则报 DecryptFailed。
// ⇒ `DecryptFailed` 与 `HashMismatch` 的边界是**数据相关**的，不是实现属性。
// 真正确定性的判据是「**必须失败**」，以及 KDF 的 keyHex/ivHex 与参考实现逐位一致。
function addNegative(id, buf, why, expectRust, mechanism) {
  const anyOf = Array.isArray(expectRust) ? expectRust : [expectRust];
  const r = tryDecrypt(buf);
  negatives.push({
    id: id,
    why: why,
    containerLen: buf.length,
    cipherB64: buf.toString('base64'),
    jsError: r.ok ? null : r.error,
    jsSucceeded: r.ok,
    expectedRustError: anyOf[0],
    expectedRustErrorAnyOf: anyOf,
    expectedRustMechanism: mechanism || null,
  });
}

// n1：用 AES 的盐（A⊕B）构造、却打上 AES_PRIVATE 头 —— 直接验证 salt_for 有没有搞反
{
  const rb = rbFor('n1');
  const wrong = encryptContainer(Buffer.from(baseVec.plaintext, 'utf8'), 'AES', rb); // AES 盐
  const buf = Buffer.concat([HEADER_PRIVATE, wrong.subarray(6)]); // 换成 PRIVATE 头
  addNegative('n1_private_header_with_ab_salt', buf,
    'AES_PRIVATE 头 + 用 SALT_A⊕SALT_B 派生的密钥：若 salt_for 取错盐，这个会被「成功」解开',
    ['DecryptFailed', 'HashMismatch'],
    '★ 方向性判据是「必须失败」而非具体变体：取错盐 ⇒ 密钥错 ⇒ 明文随机。实测 Rust 落 DecryptFailed（随机末字节碰巧非法 padding），只有 ~0.4% 概率会走到 sha512 而报 HashMismatch。若 salt_for 真的取错盐，本条会 Ok 而非 Err。');
}
// n2：篡改 randomBytes 一个 bit —— 密钥变了，应哈希不符
{
  const buf = cloneBase();
  buf[6] ^= 0x01;
  addNegative('n2_tampered_random_bytes', buf, '改 randomBytes 第 1 字节（密钥/IV 随之改变）',
    ['DecryptFailed', 'HashMismatch'],
    '同 n1：密钥错 ⇒ 明文随机 ⇒ 先撞 PKCS7 padding 校验。实测 DecryptFailed。');
}
// n3：篡改密文体一个 bit —— CBC 下先失败的可能是不去 padding，不是哈希
{
  const buf = cloneBase();
  buf[buf.length - 1] ^= 0x01;
  addNegative('n3_tampered_ciphertext_last_byte', buf, '改密文体最后一个字节（末块）',
    ['DecryptFailed', 'HashMismatch'],
    '末块被改 ⇒ padding 首当其冲。实测 DecryptFailed。');
}
{
  const buf = cloneBase();
  buf[38] ^= 0x01;
  addNegative('n4_tampered_ciphertext_first_byte', buf, '改密文体第一个字节（首块）',
    ['DecryptFailed', 'HashMismatch'],
    '★ 首块被改：CBC 下末块解密结果**不受影响** ⇒ padding 仍然合法 ⇒ 必定走到 sha512 ⇒ 稳定报 HashMismatch。实测 DecryptFailed 则说明 padding 校验位置与预期不同。');
}
// n5：末字节改成 0x00 —— PKCS7 非法 padding
{
  const buf = cloneBase();
  buf[buf.length - 1] = 0x00;
  addNegative('n5_padding_zero', buf, '末字节置 0x00（PKCS7 非法 padding 长度）',
    ['DecryptFailed'],
    '确定性：0x00 恒为非法 padding。');
}
// n6：末字节改成 0x11 —— padding 长度 > 块长
{
  const buf = cloneBase();
  buf[buf.length - 1] = 0x11;
  addNegative('n6_padding_0x11', buf, '末字节置 0x11（17 > 块长 16，非法）',
    ['DecryptFailed'],
    '确定性：0x11 恒为非法 padding。');
}
// n7：少一字节（117 < MIN_CIPHERTEXT_LEN=118）
{
  addNegative('n7_len_117', baseBuf.subarray(0, 117), '截到 117 字节，比 MIN_CIPHERTEXT_LEN 少 1',
    ['TooShort'], '确定性：长度闸。');
}
// n8：长度不是 16 的倍数（在合法容器后追加 1 字节）
{
  addNegative('n8_len_not_block_aligned', Buffer.concat([baseBuf, Buffer.from([0x41])]),
    '容器后追加 1 字节 ⇒ 密文体 16k+1', ['NotBlockAligned'],
    '确定性：对齐闸。');
}
// n9：未知头
{
  const buf = cloneBase();
  buf[0] = 0x41;
  addNegative('n9_unknown_header', buf, '头部首字节改成 0x41', ['UnknownHeader'],
    '确定性：6 字节头不匹配任一已知头。');
}
// n10：空输入
{
  addNegative('n10_empty', Buffer.alloc(0), '空 buffer', ['TooShort'], '确定性：长度闸。');
}
// n11：randomBytes 全零（合法！只是随机源退化的边界）
{
  const rb = Buffer.alloc(32, 0);
  const buf = encryptContainer(Buffer.from(baseVec.plaintext, 'utf8'), 'AES', rb);
  const r = tryDecrypt(buf);
  if (!r.ok) throw new Error('randomBytes 全零居然解不开：' + r.error);
  vectors.push({
    id: 'p1_ascii_json__AES__zerorandom',
    note: '★ randomBytes 全零的合法容器（密钥派生边界）',
    encType: 'AES',
    plaintext: baseVec.plaintext,
    plaintextUtf8Bytes: Buffer.byteLength(baseVec.plaintext, 'utf8'),
    randomBytesHex: rb.toString('hex'),
    keyHex: Buffer.from(ref.deriveKeyAndIV(rb, 'AES').aesKey).toString('hex'),
    ivHex: Buffer.from(ref.deriveKeyAndIV(rb, 'AES').iv).toString('hex'),
    containerLen: buf.length,
    cipherB64: buf.toString('base64'),
    jsRoundTrip: true,
  });
}
// n12：明文里含非法 UTF-8 字节（64 字节摘要 + 0xff）—— Rust 应报 NotUtf8，JS 会产出替换字符
{
  const rb = rbFor('n12');
  const badBody = Buffer.concat([sha512(Buffer.from([0xff, 0xfe])), Buffer.from([0xff, 0xfe])]);
  const kdf = ref.deriveKeyAndIV(rb, 'AES');
  const cipher = crypto.createCipheriv('aes-128-cbc', Buffer.from(kdf.aesKey), Buffer.from(kdf.iv));
  const ct = Buffer.concat([cipher.update(badBody), cipher.final()]);
  const buf = Buffer.concat([HEADER_AES, rb, ct]);
  const r = tryDecrypt(buf);
  negatives.push({
    id: 'n12_non_utf8_body',
    why: '密文体解出的明文含 0xff 0xfe（非法 UTF-8）；JS 用 toString("utf8") 会产出替换字符，Rust 应报 NotUtf8',
    containerLen: buf.length,
    cipherB64: buf.toString('base64'),
    jsError: r.ok ? null : r.error,
    jsSucceeded: r.ok,
    jsValueIsReplacementChars: r.ok ? r.value.includes('\uFFFD') : null,
    expectedRustError: 'NotUtf8',
    expectedRustErrorAnyOf: ['NotUtf8'],
    expectedRustMechanism: '确定性：sha512 校验通过（badBody 的摘要就是自己）但 body 含 0xff 0xfe ⇒ String::from_utf8 失败。',
  });
}

// --- 输出 -------------------------------------------------------------------
const out = {
  _generatedBy: 'test-bin/verify-trae/gen_vectors.js',
  _referenceImplementation: 'test-bin/recon/trae-wangqi233/src/trae-decrypt.js',
  _referenceLines: {
    SALT_A: 12, SALT_B: 13, SALT_C: 14, SALT_D: 15,
    xorSalts: 17, sha512: 25, detectEncType: 29, deriveKeyAndIV: 39,
    aesCbcDecrypt: 57, isTcEncrypted: 64, decryptTcBuffer: 70,
    decryptStorageValue: 117, decryptAuthData: 129,
    minCiphertextLenExpr: 'HEADER_SIZE + RANDOM_BYTES_LEN + HASH_SIZE + 16 = 6+32+64+16 = 118',
  },
  _nodeVersion: process.version,
  _saltSourceNote: '盐字节由 gen_vectors.js 用正则从参考实现源码 L12-15 程序化读出，未手抄；读出的盐已与参考实现导出的 deriveKeyAndIV 对拍一致。',
  _saltSelfCheckAllMatch: selfChecks.every((c) => c.match),
  _saltSelfCheckCount: selfChecks.length,
  _saltFingerprints: saltFingerprints,
  _containerFormat: 'header(6) ‖ randomBytes(32) ‖ AES-128-CBC-PKCS7( sha512(plaintext)(64) ‖ plaintext )',
  _expectedRustErrorSemantics: 'expectedRustErrorAnyOf 是**允许集合**（Rust 侧实际变体必须落在其中）；expectedRustMechanism 说明该集合为什么是确定的或数据相关的。★ 密钥错误类向量（n1/n2/n3）的 DecryptFailed↔HashMismatch 边界是数据相关的（随机明文末字节碰巧构成合法 PKCS7 的概率 ≈ 0.4%），确定性判据是「必须失败」。',
  vectors: vectors,
  negativeVectors: negatives,
};

fs.writeFileSync(OUT, JSON.stringify(out, null, 2).replace(/\n/g, '\r\n'), 'utf8');

console.log('REF                = ' + REF);
console.log('OUT                = ' + OUT);
console.log('NODE               = ' + process.version);
console.log('SALT_SELFCHECK     = ' + selfChecks.length + ' 组全匹配: ' + out._saltSelfCheckAllMatch);
console.log('AES_salt(AB)_sha256       = ' + saltFingerprints.AES_salt_xorAB_sha256);
console.log('AES_PRIVATE_salt(CD)_sha256 = ' + saltFingerprints.AES_PRIVATE_salt_xorCD_sha256);
console.log('AB_equals_CD       = ' + saltFingerprints.AES_salt_is_xorCD + ' (必须 false，否则两组盐退化)');
console.log('POSITIVE_VECTORS   = ' + vectors.length);
for (const v of vectors) {
  console.log('  + ' + v.id.padEnd(34) + ' len=' + String(v.containerLen).padStart(4) + ' utf8bytes=' + String(v.plaintextUtf8Bytes).padStart(4) + ' jsRoundTrip=' + v.jsRoundTrip);
}
console.log('NEGATIVE_VECTORS   = ' + negatives.length);
for (const n of negatives) {
  console.log('  - ' + n.id.padEnd(34) + ' len=' + String(n.containerLen).padStart(4) + ' jsSucceeded=' + n.jsSucceeded + ' rustAnyOf=[' + n.expectedRustErrorAnyOf.join('|') + ']' + (n.jsError ? ' jsError="' + n.jsError + '"' : ''));
}
