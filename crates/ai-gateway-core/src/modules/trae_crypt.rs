//! Trae 客户端 `storage.json` 中加密值的解密（AES-128-CBC）。
//!
//! 来源：`trae-wangqi233/src/trae-decrypt.js`（212 行）逐字复刻。
//!
//! ## 为什么需要它
//!
//! Trae 桌面客户端把登录态写在
//! `%APPDATA%\<产品目录>\User\globalStorage\storage.json` 里。**文件本身是明文
//! JSON**，但其中若干**值**是 base64 包着的密文 —— 尤其是
//! `iCubeAuthInfo://icube.cloudide`（JWT + refresh_token）。
//!
//! 在只读明文的实现里，`serde_json::from_str` 对这种 base64 字符串必然失败，
//! 于是「本机发现账号」在客户端开启了加密存储的机器上**静默返回空列表**：
//! 界面显示「未发现账号」，而账号其实就在磁盘上。这类失败没有任何报错，
//! 排查成本极高，所以解密是一条必须补上的旁路。
//!
//! ## 容器格式
//!
//! ```text
//! header(6) ‖ randomBytes(32) ‖ AES-128-CBC-PKCS7( sha512(plaintext)(64) ‖ plaintext )
//! ```
//!
//! - `header`：`74 63 05 10 00 00`（ASCII `tc`）或 `12 39 20 20 02 03`
//! - `randomBytes`：每次写入随机的 32 字节，**参与密钥派生**
//! - 明文前面带 64 字节 `sha512(plaintext)`，解密后**必须校验**（见下）
//!
//! ## 密钥派生
//!
//! ```text
//! salt     = SALT_A ⊕ SALT_B        (header = tc)
//!          | SALT_C ⊕ SALT_D        (header = 12 39 20 20 02 03)
//! final    = sha512( sha512(randomBytes) ‖ salt )
//! aesKey   = final[0..16]
//! iv       = final[16..32]
//! ```
//!
//! ## base64 入口的宽容度（与 Node 对齐）
//!
//! `storage.json` 里的密文值是 `Buffer.toString("base64")` 的产物，而**读取**侧
//! 参考实现用的是 `Buffer.from(value, "base64")` —— Node 的解码器很宽容
//! （URL-safe 字母表、缺补齐、混入非法字符都能解出同一份字节）。
//!
//! 本模块的**唯一** base64 入口是 `decode_base64_lenient`，它逐条对齐了实测到的
//! Node 语义，并且**入口判据与解码共用它**。这条「共用」是硬约束而不是实现细节：
//! 两处若各用一套解码器，就会出现「判据说像密文、解码却失败」的第二种不一致 ——
//! 而反过来（判据 `false` ⇒ 调用方直接 `continue`）正是**静默跳过**的来源：
//! 该键既不计入 `encrypted` 也不计入 `failed`，`has_failures()` 也报不出来。
//! 共用之后，判据为真 ⇒ 解码一定能拿到字节，后续失败只可能来自容器层，
//! 会被 `StorageDecryptStats::failed` 显式计数。
//!
//! ## 为什么必须校验哈希
//!
//! AES-CBC 没有认证标签：**用错密钥解出来的绝大多数情况仍是「合法」的字节串**
//! （PKCS#7 padding 有 1/256 概率恰好合法，而这里连 padding 都可能凑巧对）。
//! 若不校验，就会把垃圾字节当成 JSON 解析 —— 运气好报解析错，运气不好
//! 解出一个字段全错的对象，把错误数据当成真实登录态用下去。
//! 因此哈希不符时一律返回错误，**绝不静默返回垃圾**。

use aes::Aes128;
use base64::Engine as _;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use serde_json::Value;
use sha2::{Digest, Sha512};

type Aes128CbcDec = cbc::Decryptor<Aes128>;

/// 宽容 base64 解码引擎 —— 与 Node `Buffer.from(s, "base64")` 对齐。
///
/// 与严格 `general_purpose::STANDARD` 的两点差别：
/// - `decode_allow_trailing_bits`：末字符未使用的比特非零时**宽恕**（Node 亦宽恕）；
/// - `DecodePaddingMode::Indifferent`：补齐可有可无（调用方已把 `=` 归一掉）。
///
/// 只用于**解码**；编码仍走严格的 `STANDARD`（保证写出去的是规范形态）。
const LENIENT_BASE64: base64::engine::GeneralPurpose = base64::engine::GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    base64::engine::general_purpose::GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
);

/// 头部长度。
pub const HEADER_SIZE: usize = 6;
/// `randomBytes` 长度（参与密钥派生）。
pub const RANDOM_BYTES_LEN: usize = 32;
/// 明文前缀里的 sha512 摘要长度。
pub const HASH_SIZE: usize = 64;
/// AES-128 密钥长度。
pub const AES_KEY_SIZE: usize = 16;
/// CBC IV 长度。
pub const IV_SIZE: usize = 16;

/// `tc` 头（`74 63 05 10 00 00`）。
const HEADER_TC: [u8; HEADER_SIZE] = [0x74, 0x63, 0x05, 0x10, 0x00, 0x00];
/// `12 39 20 20 02 03` 头（JS 里叫 `AES_PRIVATE`，走另一对 salt）。
const HEADER_PRIVATE: [u8; HEADER_SIZE] = [18, 57, 32, 32, 2, 3];

/// 最小可用密文长度：头 + 随机数 + 摘要 + 至少一个 AES 块。
const MIN_CIPHERTEXT_LEN: usize = HEADER_SIZE + RANDOM_BYTES_LEN + HASH_SIZE + 16;

/// 加密容器类型（由头部前 6 字节决定，同时决定用哪对 salt）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncType {
    /// `74 63 05 10 00 00`（ASCII `tc`）
    Aes,
    /// `12 39 20 20 02 03`
    AesPrivate,
}

impl EncType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EncType::Aes => "AES",
            EncType::AesPrivate => "AES_PRIVATE",
        }
    }
}

/// 解密失败原因。
///
/// 全部是「这不是我们能解的东西」而不是「解出来但内容可疑」——
/// 后者在解密阶段一律按错误处理（见模块头注释）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptError {
    /// 长度不足，连头部 + 随机数 + 摘要都放不下。
    TooShort(usize),
    /// 头部不认识（既不是 `tc` 也不是 `12 39 20 20 02 03`）。
    UnknownHeader(String),
    /// base64 解码失败。
    BadBase64,
    /// 密文体不是 16 的整数倍（CBC 要求整块）。
    NotBlockAligned(usize),
    /// AES-CBC 解密或去 padding 失败。
    DecryptFailed(String),
    /// 解密后长度不足 64，放不下摘要。
    PlainTooShort(usize),
    /// ★ 摘要校验失败：密钥不对或数据损坏。**不得**当作「解密成功」继续。
    HashMismatch,
    /// 解密成功但内容不是合法 UTF-8。
    NotUtf8,
}

impl std::fmt::Display for CryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptError::TooShort(n) => {
                write!(f, "密文过短（{n} 字节，至少需要 {MIN_CIPHERTEXT_LEN}）")
            }
            CryptError::UnknownHeader(h) => write!(f, "未知加密类型（头部 {h}）"),
            CryptError::BadBase64 => write!(f, "base64 解码失败"),
            CryptError::NotBlockAligned(n) => write!(f, "密文体长度 {n} 不是 16 的整数倍"),
            CryptError::DecryptFailed(e) => write!(f, "AES-128-CBC 解密失败: {e}"),
            CryptError::PlainTooShort(n) => write!(f, "解密结果过短（{n} 字节，放不下 64 字节摘要）"),
            CryptError::HashMismatch => {
                write!(f, "sha512 校验失败 —— 密钥不对或数据已损坏")
            }
            CryptError::NotUtf8 => write!(f, "解密结果不是合法 UTF-8"),
        }
    }
}

impl std::error::Error for CryptError {}

/// 小写十六进制（本仓库没有 `hex` crate，且此处只需用于错误信息）。
fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// salt 常量
// ---------------------------------------------------------------------------
//
// 四组 64 字节常量，实际用到的只有两组异或结果。它们是**服务端/客户端共享的
// 固定盐**（不是每用户密钥），因此可以安全地写进公开仓库 —— 与
// `realms.js` 里那些 chat host 同理：是共享端点信息，不是 per-user secret。

const SALT_A: [u8; 64] = [
    82, 9, 106, 213, 48, 54, 165, 56, 191, 64, 163, 158, 129, 243, 215, 251, 124, 227, 57, 130,
    155, 47, 255, 135, 52, 142, 67, 68, 196, 222, 233, 203, 84, 123, 148, 50, 166, 194, 35, 61,
    238, 76, 149, 11, 66, 250, 195, 78, 8, 46, 161, 102, 40, 217, 36, 178, 118, 91, 162, 73,
    109, 139, 209, 37,
];

const SALT_B: [u8; 64] = [
    31, 221, 168, 51, 136, 7, 199, 49, 177, 18, 16, 89, 39, 128, 236, 95, 96, 81, 127, 169, 25,
    181, 74, 13, 45, 229, 122, 159, 147, 201, 156, 239, 160, 224, 59, 77, 174, 42, 245, 176, 200,
    235, 187, 60, 131, 83, 153, 97, 23, 43, 4, 126, 186, 119, 214, 38, 225, 105, 20, 99, 85, 33,
    12, 125,
];

const SALT_C: [u8; 64] = [
    191, 192, 216, 250, 122, 246, 220, 97, 31, 254, 98, 27, 8, 72, 71, 176, 135, 99, 96, 18, 127,
    101, 203, 104, 211, 102, 191, 125, 37, 72, 150, 156, 51, 229, 121, 35, 17, 153, 141, 177, 110,
    131, 150, 128, 172, 255, 254, 6, 18, 140, 55, 62, 236, 249, 135, 64, 135, 12, 117, 4, 89, 149,
    168, 209,
];

const SALT_D: [u8; 64] = [
    246, 204, 26, 232, 232, 70, 129, 109, 223, 146, 169, 242, 23, 241, 105, 145, 50, 196, 165, 42,
    254, 120, 3, 54, 244, 207, 209, 85, 53, 6, 138, 106, 175, 148, 31, 204, 186, 186, 165, 182,
    87, 142, 49, 10, 39, 110, 26, 154, 86, 56, 173, 125, 18, 64, 198, 225, 99, 99, 83, 82, 191,
    134, 76, 170,
];

fn xor_salts(a: &[u8; 64], b: &[u8; 64]) -> [u8; 64] {
    let mut out = [0u8; 64];
    for i in 0..64 {
        out[i] = a[i] ^ b[i];
    }
    out
}

/// 按容器类型取实际 salt（`SALT_A ⊕ SALT_B` 或 `SALT_C ⊕ SALT_D`）。
fn salt_for(enc: EncType) -> [u8; 64] {
    match enc {
        EncType::AesPrivate => xor_salts(&SALT_C, &SALT_D),
        EncType::Aes => xor_salts(&SALT_A, &SALT_B),
    }
}

// ---------------------------------------------------------------------------
// 判定与密钥派生
// ---------------------------------------------------------------------------

/// 缓冲区是否是 `tc` 加密容器（只查前两字节 `74 63`）。
///
/// 与 [`detect_enc_type`] 的分工：本函数是**廉价的初筛**（用于「这个值要不要
/// 试着解密」），`detect_enc_type` 才做完整的 6 字节校验。
pub fn is_tc_encrypted(buffer: &[u8]) -> bool {
    buffer.len() >= HEADER_SIZE && buffer[0] == 0x74 && buffer[1] == 0x63
}

/// 由头部 6 字节判定容器类型；不认识返回 `None`。
pub fn detect_enc_type(header: &[u8]) -> Option<EncType> {
    if header.len() < HEADER_SIZE {
        return None;
    }
    if header[..HEADER_SIZE] == HEADER_TC {
        return Some(EncType::Aes);
    }
    if header[..HEADER_SIZE] == HEADER_PRIVATE {
        return Some(EncType::AesPrivate);
    }
    None
}

/// 由 `randomBytes` 与容器类型派生 `(aesKey, iv)`。
///
/// `final = sha512( sha512(randomBytes) ‖ salt )`，`key = final[0..16]`、
/// `iv = final[16..32]`。
pub fn derive_key_and_iv(random_bytes: &[u8], enc: EncType) -> ([u8; AES_KEY_SIZE], [u8; IV_SIZE]) {
    let salt = salt_for(enc);
    let mut h = Sha512::new();
    h.update(random_bytes);
    let hash_of_random = h.finalize();

    let mut h2 = Sha512::new();
    h2.update(hash_of_random);
    h2.update(salt);
    let final_hash = h2.finalize();

    let mut key = [0u8; AES_KEY_SIZE];
    let mut iv = [0u8; IV_SIZE];
    key.copy_from_slice(&final_hash[..AES_KEY_SIZE]);
    iv.copy_from_slice(&final_hash[AES_KEY_SIZE..AES_KEY_SIZE + IV_SIZE]);
    (key, iv)
}

/// 解一个 `tc` 容器，返回明文（UTF-8 字符串）。
///
/// 完整校验链：长度 → 头部 → 块对齐 → AES-CBC 去 padding → **sha512 校验** → UTF-8。
pub fn decrypt_tc_buffer(buffer: &[u8]) -> Result<String, CryptError> {
    if buffer.len() < MIN_CIPHERTEXT_LEN {
        return Err(CryptError::TooShort(buffer.len()));
    }
    let enc = detect_enc_type(&buffer[..HEADER_SIZE])
        .ok_or_else(|| CryptError::UnknownHeader(to_hex(&buffer[..HEADER_SIZE])))?;

    let random_bytes = &buffer[HEADER_SIZE..HEADER_SIZE + RANDOM_BYTES_LEN];
    let encrypted = &buffer[HEADER_SIZE + RANDOM_BYTES_LEN..];
    if encrypted.is_empty() || encrypted.len() % 16 != 0 {
        return Err(CryptError::NotBlockAligned(encrypted.len()));
    }

    let (key, iv) = derive_key_and_iv(random_bytes, enc);
    let cipher = Aes128CbcDec::new_from_slices(&key, &iv)
        .map_err(|e| CryptError::DecryptFailed(format!("初始化解密器失败: {e}")))?;
    let mut buf = encrypted.to_vec();
    let plain = cipher
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| CryptError::DecryptFailed(format!("{e}")))?
        .to_vec();

    if plain.len() < HASH_SIZE {
        return Err(CryptError::PlainTooShort(plain.len()));
    }
    let stored_hash = &plain[..HASH_SIZE];
    let body = &plain[HASH_SIZE..];

    let mut h = Sha512::new();
    h.update(body);
    let computed = h.finalize();
    if computed[..] != *stored_hash {
        // ★ 绝不静默返回垃圾：AES-CBC 无认证标签，密钥错了也可能「解出」字节串
        return Err(CryptError::HashMismatch);
    }

    String::from_utf8(body.to_vec()).map_err(|_| CryptError::NotUtf8)
}

/// base64 解码后再解 [`decrypt_tc_buffer`]。
///
/// ★ 与入口判据 [`looks_like_encrypted_base64`] **共用同一个解码器**
/// [`decode_base64_lenient`] —— 这是本模块的一条硬约束，理由见模块头注释：
/// 两处各用一套解码器就会出现「判据说像密文、解码却失败」，或更糟的
/// 「解码本可成功、判据却 `false` ⇒ 调用方静默 `continue`」。
pub fn decrypt_tc_base64(value: &str) -> Result<String, CryptError> {
    let raw = decode_base64_lenient(value).ok_or(CryptError::BadBase64)?;
    decrypt_tc_buffer(&raw)
}

/// 一个字符串看起来是否像 `tc` 密文的 base64 编码。
///
/// 判据：base64 解出来后前两字节是 `74 63`。这是「要不要尝试解密」的入口，
/// **明文 JSON 绝不会命中**（`{` 的 base64 首字节不是 `74`，且 JSON 里的
/// 花括号在 base64 字母表外，被忽略后凑不出容器头）。
///
/// ★ 注意它**只**认 `tc` 头，与 JS 参考实现的 `isTcEncrypted`（L64）一致。
/// 若上游某天大量改用 `12 39 20 20 02 03` 头，用它做唯一判据会静默漏掉 ——
/// 因此实际入口请用 [`looks_like_encrypted_base64`]。
///
/// ★ task-18 起它内部改用 [`decode_base64_lenient`]（与解码同一路径）。
/// 本函数当前**没有生产调用方**（全仓库仅本模块单测与跨实现复核测试在调），
/// 保留是为了给「只认 tc 头」这条语义留一个可对照的锚点。
pub fn looks_like_tc_base64(value: &str) -> bool {
    match decode_base64_lenient(value) {
        Some(raw) => is_tc_encrypted(&raw),
        None => false,
    }
}

/// 一个字符串看起来是否像**任一**已知容器类型的 base64 编码。
///
/// 与 [`looks_like_tc_base64`] 的区别：同时认 `tc` 与 `12 39 20 20 02 03`。
/// 这是「要不要尝试解密」应该用的判据 —— 只用 `tc` 会把 AES_PRIVATE 形态的
/// 账号静默漏掉（那正是本模块要修的那类静默失败）。
///
/// ★ 它内部用 [`decode_base64_lenient`]，与 [`decrypt_tc_base64`] **同一条路径**。
/// 这条共用关系是硬约束：判据 `false` 会让调用方直接 `continue`（静默跳过），
/// 因此判据绝不允许比解码更严格。不变式：
/// **判据为真 ⇒ [`decode_base64_lenient`] 必定返回 `Some`**。
pub fn looks_like_encrypted_base64(value: &str) -> bool {
    match decode_base64_lenient(value) {
        Some(raw) => detect_enc_type(&raw).is_some(),
        None => false,
    }
}

/// 宽容 base64 解码 —— 本模块**唯一**的 base64 入口。
///
/// 参考实现（`trae-decrypt.js`）的入口一律是 `Buffer.from(value, "base64")`，
/// 而 Node 的解码器是宽容的。本函数逐条对齐实测到的 Node 语义
/// （探针 `probe_node_lenient.js`，Node v26.0.0 的真实输出）：
///
/// | 输入形态 | Node `Buffer.from(s,"base64")` | 本函数 |
/// |---|---|---|
/// | URL-safe 字母表 `-` `_` | 当作 `+` `/` | 同 |
/// | 缺失 `=` 补齐 | 自动补齐 | 同 |
/// | 混入非法 ASCII 字符（如 `!`） | 忽略该字符 | 同 |
/// | 空白 / 换行 / 制表符 | 忽略 | 同 |
/// | `=` 出现在中间 | **终止**解码，其后一律忽略 | 同 |
/// | 长度 `% 4 == 1` 的落单字符 | 丢弃（凑不满一个输出字节） | 同 |
/// | 末字符有非零 trailing bits | 宽恕 | 同 |
/// | 空输入 / 全非法字符 | 解出 0 字节，**不报错** | 同 |
/// | **非 ASCII 字符** | 按 Latin-1 截断取低 8 位 | ★ **直接判为失败** |
///
/// ★ 最后一行是**有意保留的唯一分歧**。Node 对 `"是"`（U+662F，低字节 `0x2F` = `/`）
/// 会把它当成一个 `/` 参与解码 —— 复刻它等于让普通中文句子被拆成字节流去猜容器头，
/// 属于纯粹的误伤面。真实密文是 `Buffer.toString("base64")` 的产物、**只含 ASCII**，
/// 因此拒绝非 ASCII 不会漏掉任何真实密文。
///
/// 返回 `None` 表示「这不是一份可解码的 base64」，调用方应当**跳过**该值；
/// 空输入返回 `Some(vec![])`（与 Node 一致：解出 0 字节，让下游按
/// [`CryptError::TooShort`] 而不是 [`CryptError::BadBase64`] 分类）。
fn decode_base64_lenient(value: &str) -> Option<Vec<u8>> {
    let mut cleaned = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch == '=' {
            // Node：`=` 是终止符，其后内容一律不参与解码
            break;
        }
        if !ch.is_ascii() {
            // 见函数文档：不复刻 Node 的 Latin-1 截断
            return None;
        }
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '/' => cleaned.push(ch),
            '-' => cleaned.push('+'),
            '_' => cleaned.push('/'),
            // 其余 ASCII（空白、标点等）一律忽略
            _ => {}
        }
    }

    // 长度 % 4 == 1：末尾那个字符落单，Node 直接丢弃它
    if cleaned.len() % 4 == 1 {
        cleaned.pop();
    }
    while cleaned.len() % 4 != 0 {
        cleaned.push('=');
    }

    if cleaned.is_empty() {
        return Some(Vec::new());
    }
    LENIENT_BASE64.decode(cleaned.as_bytes()).ok()
}

/// 该字符串是否**已经是明文**（无需解密）。
///
/// 逐字对应 JS 参考实现 `trae-decrypt.js` 的明文快路径判据（L151 / L179）：
/// `trim()` 后以 `{` 或 `"` 开头即视为明文 JSON。
///
/// ★ **这是必须保留的快路径**。Trae 客户端在未开启加密存储的机器上写的
/// 就是明文，若把明文也送进解密器，会白白走一遍 base64 解码失败/哈希不符，
/// 更糟的是可能把一段**恰好**能 base64 解码的明文当成密文而丢掉。
pub fn looks_like_plaintext_json(value: &str) -> bool {
    let t = value.trim_start();
    t.starts_with('{') || t.starts_with('"')
}

/// `storage.json` 解密过程的统计，用于诊断「为什么没发现账号」。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StorageDecryptStats {
    /// 检查过的字符串值数量。
    pub scanned: usize,
    /// 判定为密文（容器头匹配）的数量。
    pub encrypted: usize,
    /// 成功解密的数量。
    pub decrypted: usize,
    /// 判定为密文但解密失败的数量。
    pub failed: usize,
}

impl StorageDecryptStats {
    /// 是否存在「看起来是密文但解不开」的值 —— 这类情况必须能上报，
    /// 否则又会退化成一次静默失败。
    pub fn has_failures(&self) -> bool {
        self.failed > 0
    }
}

/// 解密 `storage.json` 对象里所有符合容器特征的字符串值。
///
/// 对应 JS 参考实现的 `decryptAllEncryptedValues`（L159-196），差异在于：
/// - **解密失败时保留原值**而不是塞一个 `{_decryptError}` 占位对象。占位对象会
///   污染下游的键名判断（例如 `icube_gtm` → `users` 一旦变成错误对象，
///   `gtm_users_evidence` 就会静默取到空表）；保留原值则退化为改动前的行为，
///   同时通过 [`StorageDecryptStats`] 把失败**显式**暴露出来。
/// - 明文字符串**原样保留**，不做任何再解析。
///
/// 返回新对象；入参不是对象时原样返回。
pub fn decrypt_storage_values(storage: &Value) -> Value {
    decrypt_storage_values_with_stats(storage).0
}

/// 与 [`decrypt_storage_values`] 相同，但额外返回统计。
pub fn decrypt_storage_values_with_stats(storage: &Value) -> (Value, StorageDecryptStats) {
    let mut stats = StorageDecryptStats::default();
    let Some(obj) = storage.as_object() else {
        return (storage.clone(), stats);
    };

    let mut out = serde_json::Map::with_capacity(obj.len());
    for (key, value) in obj {
        let Some(s) = value.as_str() else {
            // 非字符串（对象/数组/数字/布尔/null）一律原样保留
            out.insert(key.clone(), value.clone());
            continue;
        };

        stats.scanned += 1;

        // ★ 明文快路径：以 `{` / `"` 开头 ⇒ 已经是明文，直接保留
        if looks_like_plaintext_json(s) {
            out.insert(key.clone(), value.clone());
            continue;
        }

        // 与 JS 一致：过短的串不可能是密文（容器最小 118 字节 ⇒ base64 至少 160 字符）
        if s.trim().len() < 10 {
            out.insert(key.clone(), value.clone());
            continue;
        }

        // ★ 初筛必须认两种头，否则 AES_PRIVATE 形态会被静默漏掉
        if !looks_like_encrypted_base64(s) {
            out.insert(key.clone(), value.clone());
            continue;
        }

        stats.encrypted += 1;
        match decrypt_tc_base64(s) {
            Ok(plain) => {
                stats.decrypted += 1;
                // 明文本身通常又是 JSON（例如 iCubeAuthInfo 的 JWT 载荷对象）；
                // 能解析就换成结构化值，下游 `.get()` 才能正常工作。
                // 解析失败说明它只是普通字符串，保留为字符串。
                match serde_json::from_str::<Value>(plain.trim()) {
                    Ok(v) => {
                        out.insert(key.clone(), v);
                    }
                    Err(_) => {
                        out.insert(key.clone(), Value::String(plain));
                    }
                }
            }
            Err(_) => {
                // 保留原值：退化为改动前的行为，但失败已在 stats 里显式计数
                stats.failed += 1;
                out.insert(key.clone(), value.clone());
            }
        }
    }

    (Value::Object(out), stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cbc::cipher::BlockEncryptMut;
    use serde_json::json;

    type Aes128CbcEnc = cbc::Encryptor<Aes128>;

    /// 确定性 `randomBytes`（0..32），便于复现。
    fn fixed_random_bytes() -> [u8; RANDOM_BYTES_LEN] {
        let mut r = [0u8; RANDOM_BYTES_LEN];
        for (i, b) in r.iter_mut().enumerate() {
            *b = i as u8;
        }
        r
    }

    /// 按容器格式**自造密文**：`header ‖ randomBytes ‖ AES-CBC(sha512(pt) ‖ pt)`。
    fn encrypt_tc(plaintext: &[u8], enc: EncType, random_bytes: &[u8]) -> Vec<u8> {
        let mut h = Sha512::new();
        h.update(plaintext);
        let digest = h.finalize();

        let mut body = Vec::with_capacity(HASH_SIZE + plaintext.len());
        body.extend_from_slice(&digest);
        body.extend_from_slice(plaintext);
        wrap_body(&body, enc, random_bytes)
    }

    /// 把**任意** body 包成容器。
    ///
    /// 单独抽出来是为了构造「结构完全合法、padding 也对，但摘要不对」的样本 ——
    /// 那是唯一能确定性命中 [`CryptError::HashMismatch`] 的形态（见相关测试）。
    fn wrap_body(body: &[u8], enc: EncType, random_bytes: &[u8]) -> Vec<u8> {
        let (key, iv) = derive_key_and_iv(random_bytes, enc);
        let cipher = Aes128CbcEnc::new_from_slices(&key, &iv).expect("key/iv 长度固定合法");
        let mut buf = body.to_vec();
        let msg_len = buf.len();
        let pad_len = 16 - (msg_len % 16);
        buf.resize(msg_len + pad_len, 0);
        let ct = cipher
            .encrypt_padded_mut::<Pkcs7>(&mut buf, msg_len)
            .expect("缓冲区已按块扩容")
            .to_vec();

        let header = match enc {
            EncType::Aes => HEADER_TC,
            EncType::AesPrivate => HEADER_PRIVATE,
        };
        let mut out = Vec::with_capacity(HEADER_SIZE + RANDOM_BYTES_LEN + ct.len());
        out.extend_from_slice(&header);
        out.extend_from_slice(random_bytes);
        out.extend_from_slice(&ct);
        out
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// ★ 核心验收：**自造密文 → 解密还原**，两种容器类型都要过。
    #[test]
    fn 自造密文可解密还原() {
        let plain = r#"{"iCubeAuthInfo://icube.cloudide":"test-vector"}"#;
        let rb = fixed_random_bytes();

        for enc in [EncType::Aes, EncType::AesPrivate] {
            let ct = encrypt_tc(plain.as_bytes(), enc, &rb);
            assert_eq!(
                detect_enc_type(&ct[..HEADER_SIZE]),
                Some(enc),
                "{} 头部应被正确识别",
                enc.as_str()
            );
            assert_eq!(
                decrypt_tc_buffer(&ct).expect("自造密文必须能解回"),
                plain,
                "{} 自造密文应还原出原始明文",
                enc.as_str()
            );
            // base64 形态（storage.json 里就是这个形态）
            assert_eq!(
                decrypt_tc_base64(&b64(&ct)).expect("base64 形态也应能解"),
                plain
            );
        }

        // `is_tc_encrypted` 只认 `74 63` 前缀，因此**只**对 AES 容器成立。
        // 这不是缺陷而是与 JS 参考实现一致的行为（`isTcEncrypted` L64 同样
        // 只查前两字节），但意味着它**不能**当作唯一的「要不要解密」判据 ——
        // 初筛必须用 `looks_like_encrypted_base64`（认两种头）。
        let aes_ct = encrypt_tc(plain.as_bytes(), EncType::Aes, &rb);
        assert!(is_tc_encrypted(&aes_ct));
        let priv_ct = encrypt_tc(plain.as_bytes(), EncType::AesPrivate, &rb);
        assert!(
            !is_tc_encrypted(&priv_ct),
            "AES_PRIVATE 头不是 tc 前缀，is_tc_encrypted 必须为 false（与 JS 一致）"
        );
        assert!(
            looks_like_encrypted_base64(&b64(&priv_ct)),
            "但初筛必须能认出 AES_PRIVATE —— 否则这类账号会被静默漏掉"
        );
    }

    /// ★ 与 JS 参考实现（`trae-decrypt.js`）逐字节对齐：用 Node 生成的固定
    /// 向量，Rust 侧必须解出同一明文。
    ///
    /// 这条测试是「复刻得对不对」的**唯一硬证据** —— 只靠自造密文的话，
    /// 加密与解密两边同时写错（例如 salt 取反）也会自洽通过。
    #[test]
    fn 与_js_参考实现的固定向量对齐() {
        // node -e 用 trae-decrypt.js 的算法生成（randomBytes = 00..1f）
        const VEC: &str = "dGMFEAAAAAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh9spu4pDGfMayh7QqGwGYK6Zwf8ZZnFoaKKRiQrRiUW76tg4eARWrAxVWsEgqnDrfKP8fvKudaD6kJjd3GBDLDFEWEoCafwgGX/rkmpXAKC7RyYSGmCTrAo3itM82GqEcbjqujLqMBlRVMQrzSlHxwCXmdiGWUXroRegp1VOJ3Axg==";
        const EXPECT: &str = r#"{"iCubeAuthInfo://icube.cloudide":"test-vector"}"#;

        assert_eq!(
            decrypt_tc_base64(VEC).expect("JS 生成的向量必须能解开"),
            EXPECT
        );

        // 同一个向量的原始字节形态
        let raw = base64::engine::general_purpose::STANDARD
            .decode(VEC)
            .unwrap();
        assert_eq!(detect_enc_type(&raw[..HEADER_SIZE]), Some(EncType::Aes));
        assert_eq!(decrypt_tc_buffer(&raw).unwrap(), EXPECT);

        // 密钥/IV 派生也要逐字节对齐
        let (key, iv) = derive_key_and_iv(&fixed_random_bytes(), EncType::Aes);
        assert_eq!(to_hex(&key), "70bf044d2853f0a51dde9276af723096");
        assert_eq!(to_hex(&iv), "d89e403db9a79aab6fe435399bee8a0d");
    }

    /// ★ 哈希校验必须生效：**结构合法但摘要不符**时必须报错，不得返回垃圾。
    ///
    /// 这里刻意用「正确密钥 + 错误摘要」构造，因为这是唯一能**确定性**命中
    /// [`CryptError::HashMismatch`] 的形态：随便翻转密文字节时，CBC 往往先在
    /// 去 padding 那一步就失败了（见下一条测试），根本走不到哈希校验。
    #[test]
    fn 摘要不符时必须报哈希校验失败() {
        let rb = fixed_random_bytes();

        // body = sha512("A") ‖ "B" —— 摘要是 A 的，内容却是 B
        let mut h = Sha512::new();
        h.update(b"A");
        let mut body = h.finalize().to_vec();
        body.extend_from_slice(b"B");
        let ct = wrap_body(&body, EncType::Aes, &rb);

        assert_eq!(
            decrypt_tc_buffer(&ct),
            Err(CryptError::HashMismatch),
            "摘要与内容不符时必须报 HashMismatch"
        );
        assert_eq!(decrypt_tc_base64(&b64(&ct)), Err(CryptError::HashMismatch));

        // 摘要正确时同样的构造必须成功 —— 证明上面的失败确实来自摘要比对，
        // 而不是这条构造路径本身有问题
        let mut h2 = Sha512::new();
        h2.update(b"B");
        let mut ok_body = h2.finalize().to_vec();
        ok_body.extend_from_slice(b"B");
        let ok_ct = wrap_body(&ok_body, EncType::Aes, &rb);
        assert_eq!(decrypt_tc_buffer(&ok_ct).unwrap(), "B");
    }

    /// 被篡改的密文**必须报错**（不得静默返回垃圾）。
    ///
    /// ★ 这条测试记录了一个容易误判的事实：CBC 无认证标签，翻转密文字节后
    /// **先失败的通常是去 padding**，而不是哈希校验。因此断言写成「必须报错，
    /// 且错误必须是这两类之一」—— 若强行断言一定是 `HashMismatch`，
    /// 测试会因为「恰好先撞上 padding」而假红，反而掩盖真实行为。
    #[test]
    fn 密文被篡改时必须报错而不是返回垃圾() {
        // 与固定向量同源，只把密文体的一个字节翻转
        const TAMPERED: &str = "dGMFEAAAAAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh9spu4pDGfMayh7QqGwGYK6Zwf8ZWbFoaKKRiQrRiUW76tg4eARWrAxVWsEgqnDrfKP8fvKudaD6kJjd3GBDLDFEWEoCafwgGX/rkmpXAKC7RyYSGmCTrAo3itM82GqEcbjqujLqMBlRVMQrzSlHxwCXmdiGWUXroRegp1VOJ3Axg==";
        match decrypt_tc_base64(TAMPERED) {
            Err(CryptError::HashMismatch) | Err(CryptError::DecryptFailed(_)) => {}
            other => panic!("被篡改的密文必须报错，得到 {other:?}"),
        }

        // 逐个字节翻转：**任何**一处改动都必须被拒绝
        let rb = fixed_random_bytes();
        let base = encrypt_tc(b"tamper-me", EncType::Aes, &rb);
        for i in HEADER_SIZE..base.len() {
            let mut ct = base.clone();
            ct[i] ^= 0x01;
            match decrypt_tc_buffer(&ct) {
                Err(_) => {}
                Ok(v) => panic!("翻转第 {i} 字节后仍解出了内容: {v:?}"),
            }
        }

        // 翻转 randomBytes（改变密钥派生）同样必须失败
        for i in 0..RANDOM_BYTES_LEN {
            let mut ct = base.clone();
            ct[HEADER_SIZE + i] ^= 0x01;
            assert!(
                decrypt_tc_buffer(&ct).is_err(),
                "改动 randomBytes 第 {i} 字节后必须解不开（它参与密钥派生）"
            );
        }
    }

    /// 头部不认识 → `UnknownHeader`，且错误信息里带出实际头部便于排查。
    #[test]
    fn 未知头部被拒绝() {
        let rb = fixed_random_bytes();
        let mut ct = encrypt_tc(b"x", EncType::Aes, &rb);
        ct[2] = 0x99;
        match decrypt_tc_buffer(&ct) {
            Err(CryptError::UnknownHeader(h)) => {
                assert!(h.starts_with("746399"), "错误信息应带实际头部，得到 {h}");
            }
            other => panic!("未知头部应报 UnknownHeader，得到 {other:?}"),
        }
    }

    /// ★ 头部选错 salt 必须失败 —— 证明 `EncType` 真的参与了密钥派生
    /// （而不是两条分支写成了同一套）。
    ///
    /// 断言写成「必须报错」而不是「必须是 HashMismatch」：换错 salt 等于换了密钥，
    /// CBC 解出来的是随机字节，**通常在去 padding 那步就失败**（`Unpad Error`），
    /// 只有约 1/256 的概率 padding 恰好合法才会走到哈希校验。断言具体错误类型
    /// 会让这条测试变成随机假红，反而掩盖真实行为。
    #[test]
    fn 头部与_salt_不匹配时解密失败() {
        let rb = fixed_random_bytes();
        let mut ct = encrypt_tc(b"payload", EncType::Aes, &rb);
        // 把头部改成 AES_PRIVATE：密钥派生随之换成 C⊕D，解出来必然对不上
        ct[..HEADER_SIZE].copy_from_slice(&HEADER_PRIVATE);
        match decrypt_tc_buffer(&ct) {
            Err(CryptError::HashMismatch) | Err(CryptError::DecryptFailed(_)) => {}
            other => panic!("换成 AES_PRIVATE 头后必须解不开（否则说明 salt 分支没生效），得到 {other:?}"),
        }
        // 反向：AES_PRIVATE 密文套 AES 头
        let mut ct2 = encrypt_tc(b"payload", EncType::AesPrivate, &rb);
        ct2[..HEADER_SIZE].copy_from_slice(&HEADER_TC);
        match decrypt_tc_buffer(&ct2) {
            Err(CryptError::HashMismatch) | Err(CryptError::DecryptFailed(_)) => {}
            other => panic!("套上 AES 头后必须解不开，得到 {other:?}"),
        }
    }

    /// 长度与块对齐的前置检查：过短、非 16 倍数、空密文体都要给出明确错误。
    #[test]
    fn 长度与块对齐检查() {
        assert!(matches!(
            decrypt_tc_buffer(&[]),
            Err(CryptError::TooShort(0))
        ));
        assert!(matches!(
            decrypt_tc_buffer(&[0x74, 0x63]),
            Err(CryptError::TooShort(2))
        ));

        let rb = fixed_random_bytes();
        // ★ 明文必须足够长：`encrypt_tc(b"abc")` 总长恰好 118 字节（正好等于
        // MIN_CIPHERTEXT_LEN），砍 1 字节会先撞长度检查而不是对齐检查。
        // 用 20 字节明文 → 总长 134，砍 1 后 133 仍 ≥ 118，才能命中 NotBlockAligned。
        let mut ct = encrypt_tc(&[b'x'; 20], EncType::Aes, &rb);
        assert!(ct.len() > MIN_CIPHERTEXT_LEN);
        ct.pop();
        match decrypt_tc_buffer(&ct) {
            Err(CryptError::NotBlockAligned(n)) => {
                assert_eq!(n % 16, 15);
                assert!(n > 0);
            }
            other => panic!("非对齐密文应报 NotBlockAligned，得到 {other:?}"),
        }

        // 刚好够长度但没有密文体
        let mut no_body = Vec::new();
        no_body.extend_from_slice(&HEADER_TC);
        no_body.extend_from_slice(&rb);
        no_body.extend_from_slice(&[0u8; HASH_SIZE + 16]);
        // 长度够、对齐也对，但内容是全零 → 解出来是随机字节，必然失败
        // （与上一条同理：具体错误类型取决于 padding 是否恰好合法）
        match decrypt_tc_buffer(&no_body) {
            Err(CryptError::HashMismatch) | Err(CryptError::DecryptFailed(_)) => {}
            other => panic!("全零密文体必须解不开，得到 {other:?}"),
        }
    }

    /// base64 形态：垃圾输入与非 UTF-8 明文。
    ///
    /// ★ 垃圾输入的**报错分类在 task-18 后变了**：解码层改为宽容（与 Node 对齐）
    /// 之后，`"!!!not base64!!!"` 会被滤成 `notbase6` 解出 6 字节，于是报错落在
    /// **容器层** `TooShort(6)` 而不是解码层 `BadBase64`。这是「解码层宽容、
    /// 容器层严格」的必然结果，也与 Node 一致（Node 同样解出 6 字节
    /// `9e8b5b6ac7ba`，随后在容器层失败）。判据侧不受影响：这 6 字节不是容器头。
    #[test]
    fn base64_与_utf8_错误路径() {
        assert_eq!(decrypt_tc_base64("!!!not base64!!!"), Err(CryptError::TooShort(6)));
        assert_eq!(decrypt_tc_base64(""), Err(CryptError::TooShort(0)));
        assert_eq!(decrypt_tc_base64("===="), Err(CryptError::TooShort(0)));
        assert_eq!(decrypt_tc_base64("dGMF"), Err(CryptError::TooShort(3)));
        // 只有**非 ASCII**输入才落到 BadBase64（有意保留的分歧，见 decode_base64_lenient）
        assert_eq!(decrypt_tc_base64("这是一个测试"), Err(CryptError::BadBase64));

        // 明文不是合法 UTF-8 → NotUtf8（哈希仍会先通过）
        let rb = fixed_random_bytes();
        let bad = [0xffu8, 0xfe, 0xfd];
        let ct = encrypt_tc(&bad, EncType::Aes, &rb);
        assert_eq!(decrypt_tc_buffer(&ct), Err(CryptError::NotUtf8));
    }

    /// `looks_like_tc_base64` 是「要不要尝试解密」的入口，必须能区分
    /// 密文、明文 JSON、以及随便一个普通字符串。
    #[test]
    fn 密文初筛不误判明文() {
        let rb = fixed_random_bytes();
        let ct = encrypt_tc(br#"{"a":1}"#, EncType::Aes, &rb);
        assert!(looks_like_tc_base64(&b64(&ct)), "真密文应被识别");
        // 带空白也要能识别（storage.json 里可能被格式化过）
        let spaced = format!("  {}  ", b64(&ct));
        assert!(looks_like_tc_base64(&spaced));

        // 明文 JSON：不得被当成密文
        assert!(!looks_like_tc_base64(r#"{"a":1}"#));
        assert!(!looks_like_tc_base64("plain text"));
        assert!(!looks_like_tc_base64(""));
        assert!(!looks_like_tc_base64("   "));
        // 合法 base64 但不是 tc 头
        assert!(!looks_like_tc_base64(&b64(b"hello world, not encrypted")));
    }

    /// 空明文的边界：摘要仍然要校验通过。
    #[test]
    fn 空明文可往返() {
        let rb = fixed_random_bytes();
        let ct = encrypt_tc(b"", EncType::Aes, &rb);
        assert_eq!(decrypt_tc_buffer(&ct).unwrap(), "");
    }

    /// `is_tc_encrypted` 只查前两字节；`detect_enc_type` 做完整 6 字节校验。
    #[test]
    fn 初筛与完整判定的分工() {
        assert!(is_tc_encrypted(b"tc\x05\x10\x00\x00rest"));
        // 前两字节是 tc 但后四字节不对：初筛过、完整判定不过
        assert!(is_tc_encrypted(b"tcXXXX"));
        assert_eq!(detect_enc_type(b"tcXXXX"), None);
        // 长度不足
        assert!(!is_tc_encrypted(b"t"));
        assert_eq!(detect_enc_type(b"tc"), None);
        assert_eq!(detect_enc_type(&HEADER_PRIVATE), Some(EncType::AesPrivate));
        assert_eq!(EncType::Aes.as_str(), "AES");
        assert_eq!(EncType::AesPrivate.as_str(), "AES_PRIVATE");
    }

    /// salt 异或结果非零且两对互不相同（防「两组 salt 抄成同一对」）。
    #[test]
    fn 两组_salt_不相同() {
        let ab = salt_for(EncType::Aes);
        let cd = salt_for(EncType::AesPrivate);
        assert_ne!(ab, cd, "两对 salt 的异或结果不得相同");
        assert!(ab.iter().any(|&b| b != 0), "salt 不应全零");
        assert!(cd.iter().any(|&b| b != 0));
    }

    /// ★★ 项2 的接线验收：**明文路径不许退化**。
    ///
    /// 这是整个改动最关键的回归点 —— 未加密的机器上，返回值必须与
    /// 引入解密功能前**逐字节相同**。
    #[test]
    fn 明文路径不退化() {
        let rb = fixed_random_bytes();
        let plain_obj = r#"{"iCubeServerData://icube.cloudide":{"identityStr":"Free"}}"#;
        let storage = json!({
            // 明文 JSON 字符串（以 `{` 开头）—— 必须原样保留
            "iCubeServerData://icube.cloudide": plain_obj,
            // 明文 JSON 数组 —— 不以 `{` 开头，但也不是容器形态
            "some.list": "[1,2,3]",
            // 以 `"` 开头的字符串字面量（JS 快路径的第二种形态）
            "quoted": "\"hello\"",
            // 普通短字符串
            "short": "abc",
            // 非字符串值一律原样保留
            "num": 42,
            "flag": true,
            "nullv": null,
            "obj": {"nested": 1},
            "arr": [1, 2],
            // 一段恰好是合法 base64 但不是容器头的串（最容易被误伤）
            "b64ish": b64(b"hello world, not encrypted at all"),
        });
        let before = storage.clone();
        let (after, stats) = decrypt_storage_values_with_stats(&storage);

        assert_eq!(
            after, before,
            "明文/非字符串值必须逐字节原样保留，实际: {after}"
        );
        assert_eq!(stats.encrypted, 0, "不应把任何明文误判为密文");
        assert_eq!(stats.decrypted, 0);
        assert_eq!(stats.failed, 0);
        assert!(!stats.has_failures());

        // 明文快路径判据本身
        assert!(looks_like_plaintext_json("  {\"a\":1}"));
        assert!(looks_like_plaintext_json("\"str\""));
        assert!(!looks_like_plaintext_json("[1,2]"));
        assert!(!looks_like_plaintext_json("plain"));
        assert!(!looks_like_plaintext_json(""));
        let _ = rb;
    }

    /// ★★ 项2 端到端：**自造密文写进 storage 对象 → 解密后被下游读到**。
    ///
    /// 同时覆盖两种容器头，以及「解密后是 JSON 对象」与「解密后是普通字符串」
    /// 两条下游形态。
    #[test]
    fn storage_层可解密两种容器且不误伤明文() {
        let rb = fixed_random_bytes();

        // 形态一：解密出来是 JSON 对象（真实场景：iCubeServerData）
        let secret_obj = r#"{"entitlementInfo":{"identityStr":"Pro"}}"#;
        let ct_a = encrypt_tc(secret_obj.as_bytes(), EncType::Aes, &rb);

        // 形态二：AES_PRIVATE 头，解密出来是普通字符串（非 JSON）
        let ct_b = encrypt_tc(b"just-a-token", EncType::AesPrivate, &rb);

        // 形态三：能认出是密文但**解不开**（改了一个字节）
        let mut bad = encrypt_tc(br#"{"x":1}"#, EncType::Aes, &rb);
        let n = bad.len();
        bad[n - 1] ^= 0x01;

        let storage = json!({
            "iCubeServerData://icube.cloudide": b64(&ct_a),
            "iCubeAuthInfo://icube.cloudide": b64(&ct_b),
            "broken.value": b64(&bad),
            "untouched": "plain-value",
        });

        let (out, stats) = decrypt_storage_values_with_stats(&storage);

        // 形态一：解密 + 二次解析成对象 ⇒ 下游 `.get("entitlementInfo")` 可用
        assert_eq!(
            out["iCubeServerData://icube.cloudide"]["entitlementInfo"]["identityStr"],
            json!("Pro"),
            "解密后的 JSON 必须被解析成结构化值，否则下游 .get() 取不到"
        );
        // 形态二：AES_PRIVATE 也能解；明文不是 JSON ⇒ 保留为字符串
        assert_eq!(out["iCubeAuthInfo://icube.cloudide"], json!("just-a-token"));
        // 形态三：解不开时**保留原值**（退化为改动前行为），且必须被计数
        assert_eq!(
            out["broken.value"], storage["broken.value"],
            "解不开时必须保留原值，不得塞错误占位对象"
        );
        // 明文不受影响
        assert_eq!(out["untouched"], json!("plain-value"));

        assert_eq!(stats.scanned, 4);
        assert_eq!(stats.encrypted, 3, "三种容器形态都应被初筛认出");
        assert_eq!(stats.decrypted, 2);
        assert_eq!(stats.failed, 1);
        assert!(stats.has_failures(), "失败必须能被上报，否则又是静默失败");
    }

    /// 非对象入参原样返回（不 panic）。
    #[test]
    fn storage_非对象入参原样返回() {
        for v in [json!(null), json!(42), json!("s"), json!([1, 2])] {
            let (out, stats) = decrypt_storage_values_with_stats(&v);
            assert_eq!(out, v);
            assert_eq!(stats, StorageDecryptStats::default());
        }
    }

    // ------------------------------------------------------------------
    // task-18 修复 2：base64 入口宽容度（判据与解码共用同一条宽容路径）
    // ------------------------------------------------------------------

    /// 把标准 base64 串改写成 URL-safe 字母表。
    fn to_url_safe(s: &str) -> String {
        s.replace('+', "-").replace('/', "_")
    }

    /// 找一条**同时含 `+` 与 `/` 且带 `=` 补齐**的标准密文串。
    ///
    /// 三个条件缺一不可：不含 `+`/`/` 时 URL-safe 变体与标准形态一模一样，
    /// 不带 `=` 时「去补齐」变体也一模一样 —— 那样测试会退化成空转。
    /// 容器总长 = 38 + 16j，故 `j ≡ 0 (mod 3)` 才有 2 个补齐、`j ≡ 2` 才有 1 个；
    /// 这里用变长明文去扫，而不是赌某一条刚好合适。
    fn find_variant_seed(enc: EncType, rb: &[u8]) -> (String, String) {
        for k in 0..96usize {
            let plain = format!("{{\"k\":\"{}\"}}", "x".repeat(k));
            let std = b64(&encrypt_tc(plain.as_bytes(), enc, rb));
            if std.contains('+') && std.contains('/') && std.contains('=') {
                return (plain, std);
            }
        }
        panic!("{}: 扫不出同时含 + / 与补齐的向量", enc.as_str());
    }

    /// ★★ 验收：三类非标准形态必须与标准形态解出**逐字节相同**的明文，
    /// 且入口判据必须同样为真 —— 这同时证明了「判据与解码共用一条路径」。
    #[test]
    fn 宽容解码三类变体与标准形态逐字节相同() {
        let rb = fixed_random_bytes();

        for enc in [EncType::Aes, EncType::AesPrivate] {
            let (plain, std) = find_variant_seed(enc, &rb);
            let mid = std.len() / 2;
            let variants = [
                ("std", std.clone()),
                ("urlsafe_alphabet", to_url_safe(&std)),
                ("unpadded", std.trim_end_matches('=').to_string()),
                ("stray_char", format!("{}!{}", &std[..mid], &std[mid..])),
                (
                    "embedded_whitespace",
                    format!("{}\r\n\t {}", &std[..mid], &std[mid..]),
                ),
            ];

            for (id, value) in &variants {
                if *id != "std" {
                    assert_ne!(value, &std, "★ {id}: 变体与标准形态相同，测试无效");
                }
                assert!(
                    looks_like_encrypted_base64(value),
                    "★ {} / {id}: 入口判据必须为真（判据与解码须共用同一条宽容路径）",
                    enc.as_str()
                );
                let got = decrypt_tc_base64(value).unwrap_or_else(|e| {
                    panic!("★ {} / {id}: 宽容解码后仍失败: {e:?}", enc.as_str())
                });
                assert_eq!(
                    got.as_bytes(),
                    plain.as_bytes(),
                    "★ {} / {id}: 明文不是逐字节相同",
                    enc.as_str()
                );
                println!(
                    "[lenient] {:<12} {id:<20} 明文逐字节相同（{} B）",
                    enc.as_str(),
                    got.len()
                );
            }
        }
    }

    /// ★★ 验收：**不变式**「判据为真 ⇒ 解码必定拿得到字节」。
    ///
    /// 这正是 task-18 要修的那条静默失败的反面：原实现里判据与解码各用一套
    /// 严格解码器，解码失败 ⇒ 判据 `false` ⇒ 调用方 `continue`，该键既不计入
    /// `encrypted` 也不计入 `failed`。本测试对一张**混合输入表**逐条验证：
    /// - 判据为真时 `decode_base64_lenient` 必须返回 `Some`；
    /// - 判据为假时，绝不可能是「解得出字节却判 false」（那才是静默跳过）。
    #[test]
    fn 判据为真则解码必定成功() {
        let rb = fixed_random_bytes();
        let mut inputs: Vec<String> = Vec::new();

        for enc in [EncType::Aes, EncType::AesPrivate] {
            let (_, std) = find_variant_seed(enc, &rb);
            let mid = std.len() / 2;
            inputs.push(std.clone());
            inputs.push(to_url_safe(&std));
            inputs.push(std.trim_end_matches('=').to_string());
            inputs.push(format!("{}!{}", &std[..mid], &std[mid..]));
            inputs.push(format!("{}\n {}", &std[..mid], &std[mid..]));
            // 截断到各种长度（含 % 4 == 1 的落单形态）
            for n in [1usize, 2, 3, 5, 6, 7, 9, 17, 33, 65] {
                if n <= std.len() {
                    inputs.push(std[..n].to_string());
                }
            }
            inputs.push(String::new());
            inputs.push("====".into());
            inputs.push("!!!!".into());
            inputs.push("dGMF".into());
            inputs.push("dGMFEAA".into());
            inputs.push("这是一个测试".into());
            inputs.push("{}".into());
        }

        let mut judged_true = 0usize;
        let mut judged_false = 0usize;
        for s in &inputs {
            let decoded = decode_base64_lenient(s);
            let judged = looks_like_encrypted_base64(s);
            if judged {
                assert!(
                    decoded.is_some(),
                    "★ 不变式被破坏：判据说像密文，解码却拿不到字节: {s:?}"
                );
                judged_true += 1;
            } else {
                judged_false += 1;
            }
            // 判据必须与解码**同源**：逐字节复算一遍，任何分叉都会在这里暴露
            let recomputed = match decoded.as_ref() {
                Some(raw) => detect_enc_type(raw).is_some(),
                None => false,
            };
            assert_eq!(
                judged, recomputed,
                "★ 判据与解码不是同一条路径: {s:?}"
            );
        }
        assert!(judged_true >= 4, "至少应有几条被判为密文，实际 {judged_true}");
        assert!(judged_false >= 8, "至少应有几条被判为非密文，实际 {judged_false}");
        println!(
            "[lenient] 不变式检查 {} 条输入：判真 {judged_true} / 判假 {judged_false}，0 处分叉",
            inputs.len()
        );
    }

    /// ★ 验收：**标准形态的行为逐字不变**。
    ///
    /// 对每一条规范 base64 输入，宽容解码器与原来的严格 `STANDARD` 解码器必须
    /// 产出**逐字节相同**的缓冲。这是「放宽入口没有顺带改动标准路径」的机器判据。
    #[test]
    fn 宽容解码对标准形态逐字节不变() {
        let rb = fixed_random_bytes();
        let mut samples: Vec<String> = Vec::new();
        for enc in [EncType::Aes, EncType::AesPrivate] {
            for plain in [
                "",
                "x",
                "just-a-token",
                r#"{"a":1}"#,
                r#"{"iCubeAuthInfo://icube.cloudide":"JWT.PAYLOAD.SIG"}"#,
            ] {
                samples.push(b64(&encrypt_tc(plain.as_bytes(), enc, &rb)));
            }
        }
        assert_eq!(samples.len(), 10);

        for s in &samples {
            let strict = base64::engine::general_purpose::STANDARD
                .decode(s.as_bytes())
                .expect("标准形态必须能被严格解码器解开");
            let lenient = decode_base64_lenient(s).expect("标准形态必须能被宽容解码器解开");
            assert_eq!(lenient, strict, "★ 标准形态在宽容解码器下字节变了: {s}");
        }
        // 带空白包裹的标准形态同样不变
        let spaced = format!("  \r\n{}\t ", samples[0]);
        assert_eq!(
            decode_base64_lenient(&spaced).expect("带空白也应解开"),
            base64::engine::general_purpose::STANDARD
                .decode(samples[0].as_bytes())
                .unwrap()
        );
        println!(
            "[lenient] 标准形态 {} 条：宽容解码 == 严格解码（逐字节）",
            samples.len()
        );
    }

    /// 逐条对齐实测到的 Node `Buffer.from(s,"base64")` 边界语义
    /// （探针 `probe_node_lenient.js`，Node v26.0.0 的真实输出）。
    #[test]
    fn 宽容解码对齐_node_边界语义() {
        let dgmf = vec![0x74u8, 0x63, 0x05]; // "dGMF" -> 74 63 05
        let dgmf5 = vec![0x74u8, 0x63, 0x05, 0x10, 0x00]; // "dGMFEAA" -> 5 字节
        let dgmf6 = vec![0x74u8, 0x63, 0x05, 0x10]; // "dGMFEA" -> 4 字节

        // `=` 是终止符：其后内容一律不参与解码
        assert_eq!(decode_base64_lenient("dGMFEAA"), Some(dgmf5.clone()));
        assert_eq!(decode_base64_lenient("dGMFEAA="), Some(dgmf5.clone()));
        assert_eq!(decode_base64_lenient("dGMFEAA=="), Some(dgmf5.clone()));
        assert_eq!(decode_base64_lenient("dGMFEAA=x"), Some(dgmf5.clone()));
        assert_eq!(decode_base64_lenient("dGMFEAA=xAAAA"), Some(dgmf5.clone()));
        assert_eq!(decode_base64_lenient("dGMFEAA==AAAA"), Some(dgmf5.clone()));
        assert_eq!(decode_base64_lenient("dGMF=EAAx"), Some(dgmf.clone()));
        assert_eq!(decode_base64_lenient("=dGMFEAA"), Some(Vec::new()));
        assert_eq!(decode_base64_lenient("===="), Some(Vec::new()));

        // 长度 % 4 == 1 的落单字符被丢弃
        assert_eq!(decode_base64_lenient("A"), Some(Vec::new()));
        assert_eq!(decode_base64_lenient("dGMFB"), Some(dgmf.clone()));
        assert_eq!(decode_base64_lenient("dGMFA"), Some(dgmf.clone()));
        // % 4 == 2 / 3 正常补齐
        assert_eq!(decode_base64_lenient("dGMFEA"), Some(dgmf6.clone()));
        assert_eq!(decode_base64_lenient("EA"), Some(vec![0x10u8]));
        assert_eq!(decode_base64_lenient("EAA"), Some(vec![0x10u8, 0x00]));

        // 末字符非零 trailing bits：Node 宽恕，我方也必须宽恕
        assert_eq!(decode_base64_lenient("dGMFEB"), Some(dgmf6.clone()));
        assert_eq!(decode_base64_lenient("dGMFC"), Some(dgmf.clone()));
        assert_eq!(decode_base64_lenient("dGMFD"), Some(dgmf.clone()));

        // 空输入 / 全非法字符：解出 0 字节且**不报错**（与 Node 一致）
        assert_eq!(decode_base64_lenient(""), Some(Vec::new()));
        assert_eq!(decode_base64_lenient("   "), Some(Vec::new()));
        assert_eq!(decode_base64_lenient("!!!!"), Some(Vec::new()));

        // 非法 ASCII 字符被忽略，但**不改变**其余字节
        assert_eq!(
            decode_base64_lenient("dG!MF*EA\tA"),
            Some(dgmf5.clone()),
            "★ 忽略非法字符后必须解出同一份字节"
        );
        // URL-safe 与标准字母表等价
        assert_eq!(
            decode_base64_lenient("-_"),
            decode_base64_lenient("+/"),
            "★ URL-safe 字母表必须与标准字母表等价"
        );
        // 对照：`-` 参与解码（Node 把 `-` 当 `+`），所以它会多出一个字符
        assert_eq!(decode_base64_lenient("3.2.1-beta"), decode_base64_lenient("321+beta"));
        println!("[lenient] Node 边界语义 26 条断言全部对齐");
    }

    /// ★★ 验收：放宽后**不得误伤明文**。
    ///
    /// 语料取真实会出现在 `storage.json` 里的字符串形态（路径 / URL / 中文 /
    /// JWT / PEM / 恰好是合法 base64 的普通文本等）。任何一条被误判成密文，
    /// 都会让明文值被丢进解密器而损坏。
    #[test]
    fn 宽容解码不误判明文语料() {
        let corpus: [(&str, &str); 16] = [
            ("win_path", r"C:\Users\someone\AppData\Roaming\Trae CN\User\globalStorage\storage.json"),
            ("posix_path", "/usr/local/share/trae/resources/app/out/vs/code/electron-main/main.js"),
            ("ascii_sentence", "the quick brown fox jumps over the lazy dog and then goes home"),
            ("url", "https://www.trae.cn/ide/landing?utm_source=desktop&utm_medium=app"),
            ("uuid", "8f14e45f-ceea-467a-9c1c-1c0f0f0f0f0f"),
            ("base64ish_plain", "aGVsbG8gd29ybGQgdGhpcyBpcyBub3QgZW5jcnlwdGVkIGF0IGFsbA=="),
            ("chinese_sentence", "这是一个测试中文句子不应被误判"),
            ("jwt_like", "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"),
            ("version", "3.2.1-beta.4+build.20260930"),
            ("email", "someone@example.com"),
            ("ticket", "TICKET-1234-5678-ABCD-EFGH"),
            ("number_str", "1788081577344000"),
            ("mixed_cn_ascii", "登录失败: token expired (code=401)"),
            ("starts_tc_text", "tcheading text here and more text to make it long enough"),
            ("pem_like", "-----BEGIN RSA PRIVATE KEY-----MIIEowIBAAKCAQEA"),
            ("registry_key", r"HKEY_CURRENT_USER\Software\Trae\Settings"),
        ];

        for (id, s) in corpus {
            assert!(
                !looks_like_encrypted_base64(s),
                "★ {id} 被误判成密文: {s:?}"
            );
            assert!(!looks_like_tc_base64(s), "★ {id} 被 tc 判据误判: {s:?}");
            // 不得解密成功。失败分类按输入是否含非 ASCII 分两种，这正是
            // 「解码层宽容、容器层严格」加上「非 ASCII 有意分歧」的必然结果：
            // - 纯 ASCII 语料：解码层宽容 ⇒ 一定能拿到字节，失败必然落在
            //   **容器层**（TooShort / UnknownHeader）；
            // - 含非 ASCII 语料：`decode_base64_lenient` 有意返回 None ⇒ 解码层
            //   `BadBase64`。这一条**不是**漏网：判据侧同样为 false，调用方会
            //   照常跳过，不会产生任何计数。
            let err = decrypt_tc_base64(s)
                .err()
                .unwrap_or_else(|| panic!("★ {id} 竟然被解密成功: {s:?}"));
            let v = format!("{err:?}");
            if s.is_ascii() {
                assert!(
                    v.starts_with("TooShort") || v.starts_with("UnknownHeader"),
                    "★ {id} 的失败落在容器层之外: {v}"
                );
            } else {
                assert_eq!(v, "BadBase64", "★ {id} 含非 ASCII，应在解码层被拒: {v}");
            }
        }

        // storage 层：整份语料放进去，必须**一个都不计数**
        let obj: serde_json::Map<String, Value> = corpus
            .iter()
            .map(|(id, s)| (id.to_string(), Value::String(s.to_string())))
            .collect();
        let storage = Value::Object(obj);
        let (out, stats) = decrypt_storage_values_with_stats(&storage);
        assert_eq!(stats.scanned, corpus.len(), "语料应全部被扫描");
        assert_eq!(stats.encrypted, 0, "★ 不得把任何明文语料误判为密文");
        assert_eq!(stats.decrypted, 0);
        assert_eq!(stats.failed, 0);
        assert!(!stats.has_failures());
        assert_eq!(out, storage, "明文语料必须逐字节原样保留");
        println!("[lenient] 明文语料 {} 条：误判 0 条", corpus.len());
    }

    /// ★ 有意保留的**唯一分歧**：非 ASCII 输入我们直接判为非密文。
    ///
    /// Node 会按 Latin-1 截断取低 8 位（`"是"` → `0x2F` = `/`），复刻它等于让
    /// 普通中文被拆成字节流去猜容器头。真实密文只含 ASCII，故此分歧不漏真实数据。
    #[test]
    fn 非_ascii_输入判为非密文() {
        for s in [
            "这是一个测试",
            "dGMF是EAA",
            "登录失败: token expired",
            "ＡＢＣ",
        ] {
            assert_eq!(decode_base64_lenient(s), None, "★ {s:?} 应判为不可解码");
            assert!(!looks_like_encrypted_base64(s), "★ {s:?} 不得被判为密文");
            assert!(!looks_like_tc_base64(s));
            assert_eq!(decrypt_tc_base64(s), Err(CryptError::BadBase64));
        }
        // 对照：把非 ASCII 换成 ASCII 后，同一形态是能解开的
        assert_eq!(
            decode_base64_lenient("dGMFEAA"),
            Some(vec![0x74u8, 0x63, 0x05, 0x10, 0x00])
        );
        println!("[lenient] 非 ASCII 分歧：4 条全部判为非密文（Node 会做 Latin-1 截断）");
    }
}
