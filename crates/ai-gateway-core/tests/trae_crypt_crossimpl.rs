//! 批次3-B 独立对抗性复核：把 Trae 解密实现与**真实 Node 参考实现**做跨实现交叉验证。
//!
//! ## 为什么需要这个文件
//!
//! `trae_crypt.rs` 自带的单元测试用的是**手工抄写的 base64 常量**。那验证的是
//! 「常量抄对了没有」，**不是**「两个实现真的互操作」。本文件换一个方向：
//!
//! 1. 向量由 `test-bin/verify-trae/gen_vectors.js` **调用真实参考实现**
//!    （`test-bin/recon/trae-wangqi233/src/trae-decrypt.js`）生成 —— 盐值是从参考
//!    实现源码里**程序化读出**的（不是手抄），KDF 也与 `ref.deriveKeyAndIV` 对拍过；
//! 2. 每条正向向量都经参考实现自身往返（`jsRoundTrip === true`）；
//! 3. 本文件只做**单向**断言：Node 生成的密文 → Rust 解出的明文必须**逐字节相同**。
//!
//! ## 复核者立场
//!
//! 本文件是复核产物，**不改动** `trae_crypt.rs` / `trae_discover.rs` 等被测源码。
//! 发现的不一致（例如 Node 与 Rust 的错误分类差异）以**断言 + 记录**的方式暴露，
//! 而不是把断言放宽去迁就实现。
//!
//! ## 覆盖的两种容器头
//!
//! - `AES`：`74 63 05 10 00 00`，密钥盐 = `SALT_A ⊕ SALT_B`
//! - `AES_PRIVATE`：`12 39 20 20 02 03`，密钥盐 = `SALT_C ⊕ SALT_D`
//!
//! ★ `salt_for` 有没有把两组盐搞反，是本次复核的重点之一（见
//! [`salt_for_uses_cd_for_aes_private_not_swapped`]）。

use ai_gateway_core::modules::trae_crypt::{
    CryptError, EncType, decrypt_storage_values, decrypt_storage_values_with_stats,
    decrypt_tc_base64, decrypt_tc_buffer, derive_key_and_iv, detect_enc_type, is_tc_encrypted,
    looks_like_encrypted_base64, looks_like_plaintext_json, looks_like_tc_base64,
};
use ai_gateway_core::modules::trae_discover::app_data_dirs;
use base64::Engine as _;
use serde_json::Value;
use std::path::PathBuf;

// ---------------------------------------------------------------- 夹具位置
//
// ★★ 2026-10-09：这些夹具此前指向 `../../../test-bin/verify-trae/` —— 而
// `test-bin/` 在 `.gitignore` 里（`/test-bin/`），**从不进仓库**。后果是：
//
//   · 开发机上它们一直在（生成器跑过），本文件 9 条测试**全绿**；
//   · CI 上 checkout 出来根本没有 `test-bin/` ⇒ 7 条测试红（另 2 条不读夹具）；
//   · 于是 `ci.yml` 的 windows-latest 腿与 `release.yml` 的 preflight 腿
//     **从 1.0.23 起一直失败**，而发版照样手工进行 ⇒ 没人发现。
//
// 修法：把**输入夹具**（由生成器产出、已逐字节核对过不含任何真实账号/凭证）
// 随源码分发，放在 `tests/fixtures/trae-crypt/`。生成器与参考实现仍在
// `test-bin/`（它们依赖本机抓包产物，不适合进仓库），
// 但**测试不再依赖它们**。
//
// ⚠ `rust_adversarial_table.json` 是**本测试的输出**（`adversarial_probe_…`
// 每次跑都重写），不是输入 ⇒ 它继续落在 `test-bin/`（那里被 gitignore，
// 正合适：不该让测试往仓库里写文件）。
/// 向量文件（相对 `CARGO_MANIFEST_DIR` = `wt-port/crates/ai-gateway-core`）。
const VECTORS_REL: &str = "tests/fixtures/trae-crypt/vectors.json";
/// `storage.json` 夹具（同样由参考实现真调用产出）。
const FIXTURE_REL: &str = "tests/fixtures/trae-crypt/storage_fixture.json";
/// 对抗探测表落盘位置 —— **输出**，仍留在被 gitignore 的 `test-bin/` 下。
const TABLE_REL: &str = "../../../test-bin/verify-trae/rust_adversarial_table.json";
/// base64 入口宽容度对照表（Node 宽松 vs Rust 严格，见第 8 组测试）。
const BASE64_REL: &str = "tests/fixtures/trae-crypt/base64_variants.json";

// ---------------------------------------------------------------- helpers

fn repo_file(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn load_json(rel: &str) -> Value {
    let p = repo_file(rel);
    let text = std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("读不到 {}: {e}（生成器是否已跑过？）", p.display()));
    serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .unwrap_or_else(|e| panic!("{} 不是合法 JSON: {e}", p.display()))
}

fn b64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .unwrap_or_else(|e| panic!("向量里的 base64 解不开: {e}"))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn enc_of(s: &str) -> EncType {
    match s {
        "AES" => EncType::Aes,
        "AES_PRIVATE" => EncType::AesPrivate,
        other => panic!("向量里出现未知 encType: {other}"),
    }
}

/// `CryptError` → 变体名。
///
/// ★ 这里**故意不写 `_` 兜底分支**：一旦上游新增变体，本文件会编译失败，
/// 从而强制复核者重新审视分类表，而不是静默地把新变体归到旧桶里。
fn variant(e: &CryptError) -> &'static str {
    match e {
        CryptError::TooShort(_) => "TooShort",
        CryptError::UnknownHeader(_) => "UnknownHeader",
        CryptError::BadBase64 => "BadBase64",
        CryptError::NotBlockAligned(_) => "NotBlockAligned",
        CryptError::DecryptFailed(_) => "DecryptFailed",
        CryptError::PlainTooShort(_) => "PlainTooShort",
        CryptError::HashMismatch => "HashMismatch",
        CryptError::NotUtf8 => "NotUtf8",
    }
}

/// 在**静音 panic hook** 下运行 `f`，把「是否 panic」变成可断言的返回值。
///
/// Rust 的切片越界是 **panic 而不是 `Err`**，所以「不 panic」必须显式测。
/// 静音 hook 只影响打印，测试框架照样会把 panic 记为失败；这里只为避免
/// 几百条探测产生几百行 `thread ... panicked at ...` 噪声。
fn probe<T>(f: impl FnOnce() -> T) -> Result<T, ()> {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    std::panic::set_hook(prev);
    r.map_err(|_| ())
}

/// 一条对抗探测记录。
#[derive(Debug, Clone)]
struct ProbeRow {
    label: String,
    len: usize,
    outcome: String,
}

fn record(rows: &mut Vec<ProbeRow>, label: impl Into<String>, input: &[u8]) {
    let label = label.into();
    let len = input.len();
    let outcome = match probe(|| decrypt_tc_buffer(input)) {
        Err(()) => "★PANIC".to_string(),
        Ok(Ok(s)) => format!("Ok(len={})", s.len()),
        Ok(Err(e)) => variant(&e).to_string(),
    };
    rows.push(ProbeRow { label, len, outcome });
}

// ---------------------------------------------------------------- 1. 盐值方向

/// ★ 重点复核：`AES_PRIVATE` 必须走 `SALT_C ⊕ SALT_D`，其它走 `SALT_A ⊕ SALT_B`。
///
/// 判据不是「读代码看着对」，而是**用参考实现生成的两种头密文去解**：
/// - 用 `SALT_A ⊕ SALT_B` 加密、却挂 `AES_PRIVATE` 头的容器（向量
///   `n1_private_header_with_ab_salt`）**必须解不开**；若 `salt_for` 取错盐，
///   它会被「成功」解开 —— 这是能真正证伪的方向性检查。
/// - 同一明文、同样随机字节，换成正确的 `AES_PRIVATE` 头必须能解开。
#[test]
fn salt_for_uses_cd_for_aes_private_not_swapped() {
    let v = load_json(VECTORS_REL);

    // 两组盐不能退化相同（否则方向性检查毫无意义）。
    let ab = derive_key_and_iv(&[0u8; 32], EncType::Aes);
    let cd = derive_key_and_iv(&[0u8; 32], EncType::AesPrivate);
    assert_ne!(ab.0, cd.0, "AES 与 AES_PRIVATE 的密钥相同 ⇒ 两组盐退化");
    assert_ne!(ab.1, cd.1, "AES 与 AES_PRIVATE 的 IV 相同 ⇒ 两组盐退化");

    // 拿同一 randomBytes 的两种头向量，断言它们派生出不同密钥。
    let vecs = v["vectors"].as_array().expect("vectors 数组");
    let a = vecs
        .iter()
        .find(|x| x["id"] == "p1_ascii_json__AES")
        .expect("p1_ascii_json__AES");
    let b = vecs
        .iter()
        .find(|x| x["id"] == "p1_ascii_json__AES_PRIVATE")
        .expect("p1_ascii_json__AES_PRIVATE");
    assert_eq!(a["plaintext"], b["plaintext"], "两条向量应同明文");
    assert_ne!(
        a["keyHex"], b["keyHex"],
        "同 randomBytes 下两种头的密钥必须不同"
    );

    // 方向性证伪：AES_PRIVATE 头 + A⊕B 盐 ⇒ 必须失败。
    let n1 = v["negativeVectors"]
        .as_array()
        .expect("negativeVectors")
        .iter()
        .find(|x| x["id"] == "n1_private_header_with_ab_salt")
        .expect("n1_private_header_with_ab_salt");
    let raw = b64(n1["cipherB64"].as_str().expect("cipherB64"));
    assert_eq!(
        detect_enc_type(&raw[..6]),
        Some(EncType::AesPrivate),
        "n1 应挂 AES_PRIVATE 头"
    );
    match decrypt_tc_buffer(&raw) {
        Err(e) => {
            let got = variant(&e);
            assert!(
                got == "HashMismatch" || got == "DecryptFailed",
                "n1 期望 HashMismatch/DecryptFailed，实际 {got}"
            );
            println!("[salt_for] n1 取错盐 ⇒ 正确失败: {got}（JS 侧: {}）", n1["jsError"]);
        }
        Ok(s) => panic!(
            "★ salt_for 可能取错盐：AES_PRIVATE 头 + A⊕B 盐竟然解开了（{} 字节）",
            s.len()
        ),
    }

    // 反向对照：同明文用正确盐 + AES_PRIVATE 头必须解开。
    let good = vecs
        .iter()
        .find(|x| x["id"] == "p1_ascii_json__AES_PRIVATE")
        .expect("AES_PRIVATE 正向向量");
    let raw = b64(good["cipherB64"].as_str().expect("cipherB64"));
    assert_eq!(
        decrypt_tc_buffer(&raw).expect("正确盐 + AES_PRIVATE 头必须解开"),
        good["plaintext"].as_str().expect("plaintext")
    );
}

// ---------------------------------------------------------------- 2. 正向跨实现

/// ★★ 真·跨实现验证：Node 参考实现生成的密文，Rust 必须解出**逐字节相同**的明文。
///
/// 同时校验三件事：
/// - `detect_enc_type` 与向量标注的 `encType` 一致；
/// - `derive_key_and_iv` 算出的 key/iv 与**参考实现自己算出的** `keyHex`/`ivHex` 一致
///   （这一步把「KDF 一致」与「解密一致」分开证，出错时能立刻定位）；
/// - `decrypt_tc_buffer`（原始字节入口）与 `decrypt_tc_base64`（base64 入口）都能解出。
#[test]
fn crossimpl_positive_vectors_decrypt_byte_identical() {
    let v = load_json(VECTORS_REL);
    assert_eq!(
        v["_saltSelfCheckAllMatch"], true,
        "生成器的盐自检未通过，向量不可信"
    );

    let vecs = v["vectors"].as_array().expect("vectors 数组");
    assert_eq!(vecs.len(), 15, "正向向量数量与预期不符");

    let mut n_aes = 0usize;
    let mut n_private = 0usize;
    let mut total_bytes = 0usize;

    for x in vecs {
        let id = x["id"].as_str().expect("id");
        let enc = enc_of(x["encType"].as_str().expect("encType"));
        let plaintext = x["plaintext"].as_str().expect("plaintext");
        let raw = b64(x["cipherB64"].as_str().expect("cipherB64"));

        assert_eq!(
            raw.len(),
            x["containerLen"].as_u64().expect("containerLen") as usize,
            "{id}: 容器长度与标注不符"
        );

        // (a) 头识别
        assert_eq!(detect_enc_type(&raw[..6]), Some(enc), "{id}: 头识别错误");
        assert_eq!(enc.as_str(), x["encType"].as_str().unwrap(), "{id}: as_str");
        // `is_tc_encrypted` 只查前两字节 74 63 ⇒ 仅对 AES 头为真
        assert_eq!(
            is_tc_encrypted(&raw),
            enc == EncType::Aes,
            "{id}: is_tc_encrypted 只应认 74 63 头"
        );
        // tc 头专用入口对 AES_PRIVATE 应当为假，通用入口应当为真 —— 记录该差异
        assert_eq!(
            looks_like_tc_base64(x["cipherB64"].as_str().unwrap()),
            enc == EncType::Aes,
            "{id}: looks_like_tc_base64 只应认 tc 头"
        );
        assert!(
            looks_like_encrypted_base64(x["cipherB64"].as_str().unwrap()),
            "{id}: looks_like_encrypted_base64 应认两种头"
        );

        // (b) KDF 与参考实现逐位对齐
        let rb = hex_decode_hexstr(x["randomBytesHex"].as_str().expect("randomBytesHex"));
        assert_eq!(rb.len(), 32, "{id}: randomBytes 应 32 字节");
        let (key, iv) = derive_key_and_iv(&rb, enc);
        assert_eq!(
            hex(&key),
            x["keyHex"].as_str().expect("keyHex"),
            "{id}: 派生密钥与参考实现不一致"
        );
        assert_eq!(
            hex(&iv),
            x["ivHex"].as_str().expect("ivHex"),
            "{id}: 派生 IV 与参考实现不一致"
        );

        // (c) 解密：逐字节相同
        let out = decrypt_tc_buffer(&raw).unwrap_or_else(|e| panic!("{id}: Rust 解密失败 {e:?}"));
        assert_eq!(
            out.as_bytes(),
            plaintext.as_bytes(),
            "{id}: 明文不是逐字节相同"
        );
        assert_eq!(
            out.len(),
            x["plaintextUtf8Bytes"].as_u64().expect("plaintextUtf8Bytes") as usize,
            "{id}: 明文字节数不符"
        );

        // (d) base64 入口同样可解
        let out2 = decrypt_tc_base64(x["cipherB64"].as_str().unwrap())
            .unwrap_or_else(|e| panic!("{id}: base64 入口解密失败 {e:?}"));
        assert_eq!(out2.as_bytes(), plaintext.as_bytes(), "{id}: base64 入口不一致");

        match enc {
            EncType::Aes => n_aes += 1,
            EncType::AesPrivate => n_private += 1,
        }
        total_bytes += plaintext.len();
        println!("[positive] {id:<38} {:<12} {} B ✓", enc.as_str(), raw.len());
    }

    assert!(n_aes >= 7, "AES 头样本不足: {n_aes}");
    assert!(n_private >= 7, "AES_PRIVATE 头样本不足: {n_private}");
    assert!(total_bytes > 4000, "明文总字节数异常: {total_bytes}");
    println!("[positive] 共 {} 条全部逐字节相同（AES {n_aes} / AES_PRIVATE {n_private}）", vecs.len());
}

/// 把 `"6f7f31..."` 形式的 hex 串转成字节（避免为一个辅助函数引入 `hex` 依赖）。
fn hex_decode_hexstr(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0, "hex 串长度应为偶数: {s}");
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("hex"))
        .collect()
}

// ---------------------------------------------------------------- 3. 负向跨实现

/// 负向向量：每条都必须返回 `Err`，且**变体名落在允许集合内**。
///
/// ★ 关于「允许集合」：密钥错误的向量（`n1`/`n2`/`n3`）在 CBC 下解出的是一段
/// **随机明文**。Rust 先做 PKCS7 去 padding，只有末字节碰巧构成合法 padding
/// （概率 ≈ 0.4%）时才走到 sha512 而报 `HashMismatch`，否则报 `DecryptFailed`。
/// ⇒ 这两个变体的边界是**数据相关**的，不是实现属性；确定性判据是「**必须失败**」。
/// 该语义由向量文件里的 `expectedRustErrorAnyOf` + `expectedRustMechanism` 显式承载。
///
/// ★ 另一处真实差异：Node/OpenSSL 把 padding 错误也归到 `bad decrypt`，而 Rust
/// 能区分 `DecryptFailed` 与 `HashMismatch`。两侧错误**文案**本就不可能一致，
/// 本测试只钉 Rust 侧的变体集合，并把 JS 侧文案一并打印便于人工比对分类粒度。
#[test]
fn crossimpl_negative_vectors_error_variants_match() {
    let v = load_json(VECTORS_REL);
    let negs = v["negativeVectors"].as_array().expect("negativeVectors 数组");
    assert_eq!(negs.len(), 11, "负向向量数量与预期不符");

    let mut table = Vec::new();
    for x in negs {
        let id = x["id"].as_str().expect("id");
        let raw = b64(x["cipherB64"].as_str().expect("cipherB64"));
        let any_of: Vec<String> = x["expectedRustErrorAnyOf"]
            .as_array()
            .unwrap_or_else(|| panic!("{id}: 向量缺少 expectedRustErrorAnyOf"))
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect();
        assert!(!any_of.is_empty(), "{id}: 允许集合为空");

        let got = match probe(|| decrypt_tc_buffer(&raw)) {
            Err(()) => panic!("★ {id}: 触发了 panic（越界），而不是返回 Err"),
            Ok(Ok(s)) => panic!("★ {id}: 竟然成功解出 {} 字节，预期必须失败", s.len()),
            Ok(Err(e)) => variant(&e),
        };

        let ok = any_of.iter().any(|a| a == got);
        println!(
            "[negative] {id:<34} rust={got:<14} anyOf=[{}] js={}",
            any_of.join("|"),
            x["jsError"].as_str().unwrap_or("<JS 未报错>")
        );
        assert!(
            ok,
            "★ {id}: 变体 {got} 不在允许集合 {any_of:?} 内（机制: {}）",
            x["expectedRustMechanism"].as_str().unwrap_or("未记录")
        );
        table.push((id.to_string(), got.to_string(), any_of));
    }

    // ★ 确定性向量的落点必须唯一 —— 若某个确定性向量开始漂移，说明实现语义变了
    for (id, got, any_of) in &table {
        if any_of.len() == 1 {
            assert_eq!(
                any_of[0], *got,
                "★ {id} 是确定性向量，落点必须唯一"
            );
        }
    }
    assert_eq!(table.len(), 11);

    // ★ n4（改首块）在 CBC 下**末块明文不受影响** ⇒ padding 必定合法 ⇒ 必须走到
    //   sha512 ⇒ 必须是 HashMismatch。这是本组里最能证伪「校验顺序」的一条。
    let n4 = table
        .iter()
        .find(|(id, _, _)| id == "n4_tampered_ciphertext_first_byte")
        .expect("n4");
    assert_eq!(
        n4.1, "HashMismatch",
        "★ n4 改的是首块，末块解密结果不受影响 ⇒ padding 必然合法 ⇒ 应报 HashMismatch；\
         实际 {} 说明 padding 校验先于（或替代了）哈希校验",
        n4.1
    );
}

// ---------------------------------------------------------------- 4. 对抗探测

/// ★★ 对抗探测：穷举长度 + 逐字节篡改，断言**永不 panic**，并落盘变体表。
///
/// 重点盯 `trae_crypt.rs:248 decrypt_tc_buffer` 的切片：
/// `&buffer[..6]` / `&buffer[6..38]` / `&buffer[38..]`。只要长度闸有洞，
/// Rust 的越界就是 **panic 而不是 `Err`**，在 HTTP/Tauri 路径上会直接打挂进程。
#[test]
fn adversarial_probe_never_panics_and_variant_table() {
    let v = load_json(VECTORS_REL);
    let vecs = v["vectors"].as_array().expect("vectors");
    let base = vecs
        .iter()
        .find(|x| x["id"] == "p1_ascii_json__AES")
        .expect("基准向量");
    let base_raw = b64(base["cipherB64"].as_str().unwrap());

    let mut rows: Vec<ProbeRow> = Vec::new();

    // (1) 长度穷举：0..=200 全部截断（含 118 恰好边界、117 少一字节）
    for n in 0..=200usize {
        let n = n.min(base_raw.len());
        record(&mut rows, format!("truncate_{n}"), &base_raw[..n]);
    }

    // (2) 长度非 16 倍数（119..=135）—— 覆盖 NotBlockAligned 分支
    for n in 119..=135usize {
        record(&mut rows, format!("unaligned_{n}"), &base_raw[..n]);
    }

    // (3) 逐字节翻转（每个偏移一次，XOR 0xFF）
    for i in 0..base_raw.len() {
        let mut m = base_raw.clone();
        m[i] ^= 0xFF;
        record(&mut rows, format!("flip_{i}"), &m);
    }

    // (4) 特殊构造
    record(&mut rows, "empty", &[]);
    record(&mut rows, "all_zero_166", &vec![0u8; 166]);
    record(&mut rows, "all_ff_166", &vec![0xFFu8; 166]);
    record(&mut rows, "header_only_6", &base_raw[..6]);
    record(&mut rows, "header_plus_1", &base_raw[..7]);
    record(&mut rows, "header_plus_32", &base_raw[..38]);
    record(&mut rows, "private_header_only", &[18, 57, 32, 32, 2, 3]);
    record(&mut rows, "tc_two_bytes_only", &[0x74, 0x63]);
    let mut zero_tc = vec![0u8; 118];
    zero_tc[..6].copy_from_slice(&[0x74, 0x63, 0x05, 0x10, 0, 0]);
    record(&mut rows, "tc_header_zero_pad_118", &zero_tc);
    let mut zero_priv = vec![0u8; 118];
    zero_priv[..6].copy_from_slice(&[18, 57, 32, 32, 2, 3]);
    record(&mut rows, "private_header_zero_pad_118", &zero_priv);
    // 非 UTF-8 明文（sha512 校验通过但 UTF-8 非法）—— 需要真造密文，见第 (5) 组
    // 随机噪声（固定种子，保证可复现）
    let mut seed = 0x1234_5678_9abc_def0u64;
    for k in 0..32 {
        let mut m = base_raw.clone();
        for b in m.iter_mut().skip(6) {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *b ^= (seed >> 33) as u8;
        }
        record(&mut rows, format!("noise_{k}"), &m);
    }

    // (5) 非法 UTF-8：用 AES_PRIVATE 头的向量篡改 body 后重算 sha512 不可行（需要密钥），
    //     所以这里直接用 n12 向量（生成器已造好：sha512 校验通过但 body 非 UTF-8）。
    let n12 = v["negativeVectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == "n12_non_utf8_body")
        .expect("n12");
    let raw12 = b64(n12["cipherB64"].as_str().unwrap());
    match decrypt_tc_buffer(&raw12) {
        Err(e) => {
            let got = variant(&e);
            println!("[utf8] n12 实测变体 = {got}（JS 侧 jsSucceeded={}）", n12["jsSucceeded"]);
            assert_eq!(got, "NotUtf8", "★ n12 期望 NotUtf8，实际 {got}");
            rows.push(ProbeRow {
                label: "n12_non_utf8_body".into(),
                len: raw12.len(),
                outcome: got.to_string(),
            });
        }
        Ok(s) => panic!("★ n12 应报 NotUtf8，却成功解出 {} 字节", s.len()),
    }

    // (6) 裸字节入口之外：base64 入口的垃圾输入不得 panic
    let junk = [
        "", " ", "!!!!", "====", "dGMF", "dGMFEAA", "\u{0}\u{0}", "44GC44GC",
    ];
    let mut junk_rows = Vec::new();
    for s in junk {
        let outcome = match probe(|| decrypt_tc_base64(s)) {
            Err(()) => "★PANIC".to_string(),
            Ok(Ok(v)) => format!("Ok(len={})", v.len()),
            Ok(Err(e)) => variant(&e).to_string(),
        };
        junk_rows.push(format!("{s:?} => {outcome}"));
    }

    // ---- 汇总与断言
    let panics: Vec<&ProbeRow> = rows.iter().filter(|r| r.outcome == "★PANIC").collect();
    println!(
        "[adversarial] 共 {} 条探测；PANIC = {}",
        rows.len(),
        panics.len()
    );
    for p in &panics {
        println!("  ★PANIC {} (len={})", p.label, p.len);
    }
    assert!(
        panics.is_empty(),
        "★ 发现 {} 条 panic（越界），样本: {:?}",
        panics.len(),
        panics.iter().take(8).map(|r| &r.label).collect::<Vec<_>>()
    );

    // 变体分布
    let mut dist: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for r in &rows {
        *dist.entry(r.outcome.as_str()).or_insert(0) += 1;
    }
    println!("[adversarial] 变体分布 = {dist:?}");
    println!("[adversarial] base64 垃圾输入 = {junk_rows:?}");

    // 边界必须精确落在预期变体上
    let at = |n: usize| -> String {
        rows.iter()
            .find(|r| r.label == format!("truncate_{n}"))
            .map(|r| r.outcome.clone())
            .expect("truncate 行")
    };
    assert_eq!(at(0), "TooShort", "空输入应 TooShort");
    assert_eq!(at(117), "TooShort", "117 字节应 TooShort");
    assert_eq!(at(6), "TooShort", "仅头部应 TooShort");
    // 118 恰好等于 MIN_CIPHERTEXT_LEN ⇒ 过长度闸，但密文体为 0 字节 ⇒ NotBlockAligned(0)
    let at118 = at(118);
    assert!(
        at118 == "NotBlockAligned" || at118 == "DecryptFailed" || at118 == "HashMismatch",
        "118 字节边界落点异常: {at118}"
    );

    // 落盘对抗表（writeScope 内），失败不影响断言
    let report = serde_json::json!({
        "_generatedBy": "wt-port/crates/ai-gateway-core/tests/trae_crypt_crossimpl.rs::adversarial_probe_never_panics_and_variant_table",
        "_sourceVectors": VECTORS_REL,
        "probeCount": rows.len(),
        "panicCount": panics.len(),
        "variantDistribution": dist,
        "base64Junk": junk_rows,
        "rows": rows.iter().map(|r| serde_json::json!({
            "label": r.label, "len": r.len, "outcome": r.outcome,
        })).collect::<Vec<_>>(),
    });
    let out = repo_file(TABLE_REL);
    if let Some(dir) = out.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match serde_json::to_string_pretty(&report) {
        Ok(text) => {
            let text = text.replace('\n', "\r\n");
            match std::fs::write(&out, text) {
                Ok(()) => println!("[adversarial] 表已落盘: {}", out.display()),
                Err(e) => println!("[adversarial] 落盘失败（不影响断言）: {e}"),
            }
        }
        Err(e) => println!("[adversarial] 序列化失败（不影响断言）: {e}"),
    }
}

// ---------------------------------------------------------------- 5. storage.json 夹具

/// ★ `storage.json` 级跨实现：夹具由参考实现**真调用**产出，Rust 必须逐键产出
/// `_expect[].expectedValue`。
///
/// 同时钉住两处**刻意分歧**（Rust 优于参考实现），以及一处参考实现的**静默失败**：
/// `decryptAllEncryptedValues` 因 `isTcEncrypted` 只认 `tc` 头，会静默跳过
/// `AES_PRIVATE` 容器 —— 而它自己的单值 API 能解开同一个容器。
#[test]
fn storage_fixture_matches_expectations_and_diverges_from_js_bulk() {
    let fx = load_json(FIXTURE_REL);
    assert_eq!(fx["_syntheticOnly"], true, "夹具必须是合成值");
    let fixture = &fx["fixture"];
    let (out, stats) = decrypt_storage_values_with_stats(fixture);

    // (a) 逐键比对
    let expect = fx["_expect"].as_array().expect("_expect 数组");
    assert_eq!(expect.len(), 13, "夹具键数与预期不符");
    for e in expect {
        let key = e["key"].as_str().expect("key");
        let want = &e["expectedValue"];
        let got = out.get(key).unwrap_or_else(|| panic!("输出缺少键 {key}"));
        assert_eq!(
            got, want,
            "★ 键 {key} 不符（kind={}）: got={got} want={want}",
            e["kind"]
        );
    }
    assert_eq!(
        out.as_object().map(|o| o.len()),
        fixture.as_object().map(|o| o.len()),
        "键数量不应变化"
    );

    // (b) 统计
    let es = &fx["_expectStats"];
    assert_eq!(stats.scanned, es["scanned"].as_u64().unwrap() as usize, "scanned");
    assert_eq!(stats.encrypted, es["encrypted"].as_u64().unwrap() as usize, "encrypted");
    assert_eq!(stats.decrypted, es["decrypted"].as_u64().unwrap() as usize, "decrypted");
    assert_eq!(stats.failed, es["failed"].as_u64().unwrap() as usize, "failed");
    assert!(stats.has_failures(), "有 1 条损坏密文，has_failures 应为真");
    println!("[fixture] stats = {stats:?}");

    // (c) ★ 参考实现的静默失败：bulk API 漏掉的 AES_PRIVATE 容器，Rust 必须解出
    let missed: Vec<String> = fx["_missedByBulkButDecryptable"]
        .as_array()
        .expect("_missedByBulkButDecryptable")
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    assert!(!missed.is_empty(), "本夹具应至少含 1 条 bulk 漏掉的键");
    let js_bulk = fx["_jsStorageResults"].as_object().expect("_jsStorageResults");
    for k in &missed {
        assert!(
            !js_bulk.contains_key(k),
            "★ 参考实现 bulk 结果里不应有 {k}（它应当被静默跳过）"
        );
        let got = out.get(k).unwrap_or_else(|| panic!("Rust 输出缺少 {k}"));
        assert!(
            got.is_object() || got.is_string(),
            "★ Rust 应已解出 {k}，实际仍是未解形态: {got}"
        );
        assert!(
            got.as_str().map(|s| !s.starts_with("Ejkg")).unwrap_or(true),
            "★ {k} 仍是原始 base64，说明 Rust 也漏了 AES_PRIVATE 容器"
        );
        println!("[fixture] ★ bulk 漏掉但 Rust 解出: {k} => {got}");
    }

    // (d) 两处刻意分歧：JS 塞 {_decryptError} 占位对象，Rust 保留原值
    let mut js_err_placeholder = 0usize;
    for (k, v) in js_bulk {
        if v.get("_decryptError").is_some() {
            js_err_placeholder += 1;
            let got = out.get(k).unwrap_or_else(|| panic!("Rust 输出缺少 {k}"));
            assert!(
                got.get("_decryptError").is_none(),
                "★ Rust 不应产出 {{_decryptError}} 占位对象: {k}"
            );
            println!("[fixture] ★ 分歧 {k}: JS={v} | Rust={got}");
        }
    }
    assert!(
        js_err_placeholder >= 1,
        "参考实现本应对至少 1 个键塞占位对象（用于对照）"
    );

    // (e) 明文键必须原样保留（快路径）
    assert_eq!(
        out["iCubeServerData://icube.cloudide"],
        fixture["iCubeServerData://icube.cloudide"],
        "明文 JSON 应原样保留"
    );
    assert_eq!(
        out["plaintext.quoted"],
        fixture["plaintext.quoted"],
        "以双引号开头的明文应原样保留"
    );
}

// ---------------------------------------------------------------- 6. 明文快路径不退化

/// 明文快路径不退化：未加密机器上 `decrypt_storage_values` 必须**恒等**。
///
/// 包含对本机真实 `storage.json` 的只读检查（文件不存在时跳过并打印 SKIP，
/// 因为该测试不应在无 Trae 的机器上失败）。
#[test]
fn plaintext_fast_path_does_not_regress() {
    // (a) 纯明文对象恒等
    let plain = serde_json::json!({
        "iCubeServerData://icube.cloudide": "{\"tier\":\"pro\",\"seats\":3}",
        "plaintext.quoted": "\"just-a-string\"",
        "telemetry.machineId": "a1b2c3d4e5f6a7b8c9d0e1f2",
        "windowControlHeight": 30,
        "windowSplash": true,
        // ★ uid 用合成值：真实账号标识不得进入仓库（任务书硬约束）
        "icube_gtm": { "users": { "1883919207380040": { "gtm101_idea": { "showCount": 1 } } } }
    });
    let out = decrypt_storage_values(&plain);
    assert_eq!(out, plain, "★ 纯明文对象必须逐字节恒等");
    let (_, st) = decrypt_storage_values_with_stats(&plain);
    assert_eq!(st.encrypted, 0, "纯明文对象不应被判为密文");
    assert_eq!(st.decrypted, 0);
    assert_eq!(st.failed, 0);
    assert!(!st.has_failures());

    // (b) 幂等性：二次解密结果不变
    let fx = load_json(FIXTURE_REL);
    let once = decrypt_storage_values(&fx["fixture"]);
    let twice = decrypt_storage_values(&once);
    assert_eq!(once, twice, "★ 解密不幂等（二次运行改变了结果）");

    // (c) 快路径判定函数本身
    assert!(looks_like_plaintext_json("  {\"a\":1}"));
    assert!(looks_like_plaintext_json("\"s\""));
    assert!(!looks_like_plaintext_json("dGMFEAAA"));
    assert!(!looks_like_plaintext_json(""));

    // (d) ★ 本机真实 storage.json（只读，绝不改写）
    let dirs = app_data_dirs("Trae");
    let mut checked = 0usize;
    for d in &dirs {
        let p = d.join("User/globalStorage/storage.json");
        if !p.is_file() {
            println!("[real] SKIP 不存在: {}", p.display());
            continue;
        }
        let raw_text = std::fs::read_to_string(&p).expect("读真实 storage.json");
        let raw_json: Value =
            serde_json::from_str(raw_text.trim_start_matches('\u{feff}')).expect("解析");
        let (out, st) =
            decrypt_storage_values_with_stats(&raw_json);
        assert_eq!(
            out.as_object().map(|o| o.len()),
            raw_json.as_object().map(|o| o.len()),
            "真实文件键数不应变化: {}",
            p.display()
        );
        // 明文键（以 { 或 " 开头的字符串）必须原样保留
        let mut plain_keys = 0usize;
        for (k, v) in raw_json.as_object().expect("对象") {
            if let Some(s) = v.as_str() {
                if looks_like_plaintext_json(s) {
                    plain_keys += 1;
                    assert_eq!(
                        out.get(k),
                        Some(v),
                        "★ 真实文件的明文键 {k} 被改动了"
                    );
                }
            }
        }
        println!(
            "[real] {} 顶层键={} 明文键={} stats={st:?}",
            p.display(),
            raw_json.as_object().map(|o| o.len()).unwrap_or(0),
            plain_keys
        );
        checked += 1;
    }
    if checked == 0 {
        println!("[real] 本机无 Trae storage.json，跳过真实文件检查");
    }

    // (e) 顶层非对象入参原样返回
    let arr = serde_json::json!([1, 2, 3]);
    assert_eq!(decrypt_storage_values(&arr), arr);
    assert_eq!(decrypt_storage_values(&Value::Null), Value::Null);
}

// ---------------------------------------------------------------- 7. 生产接线

/// ★★「解密能力到底接线了没有」—— 用源码级断言给出**是/否 + 行号**，而不是口头结论。
///
/// 结论是「**已接线**，但 stats 的消费方**未接线**」：
/// - `read_storage_json`（被 `discover_app` / `read_entitlement` 调用）内部走
///   `decrypt_storage_values_with_stats` ⇒ 解密在生产路径上生效；
/// - 但 `read_storage_json` 用 `.map(|(v, _)| v)` **丢弃** stats，
///   且 `read_storage_json_with_stats` 在**全仓没有任何生产调用方**。
#[test]
fn production_wiring_read_storage_json_goes_through_decryptor() {
    let src_path = repo_file("src/modules/trae_discover.rs");
    let src = std::fs::read_to_string(&src_path)
        .unwrap_or_else(|e| panic!("读不到 {}: {e}", src_path.display()));

    let lines: Vec<&str> = src.lines().collect();
    let find = |needle: &str| -> Vec<usize> {
        lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains(needle))
            .map(|(i, _)| i + 1)
            .collect()
    };

    // (a) read_storage_json → read_storage_json_with_stats（并丢弃 stats）
    let a = find("read_storage_json_with_stats(app_kind).map(|(v, _)| v)");
    assert_eq!(a.len(), 1, "★ read_storage_json 未走 with_stats: {a:?}");
    println!("[wiring] read_storage_json 丢弃 stats 处 = trae_discover.rs:{}", a[0]);

    // (b) with_stats → trae_crypt 解密器
    let b = find("trae_crypt::decrypt_storage_values_with_stats(&v)");
    assert_eq!(b.len(), 1, "★ with_stats 未调用解密器: {b:?}");
    println!("[wiring] 调用解密器处 = trae_discover.rs:{}", b[0]);

    // (c) 生产调用方：discover_app / read_entitlement 都走 read_storage_json
    let c = find("let storage = read_storage_json(app_kind)?;");
    assert_eq!(c.len(), 1, "★ read_entitlement 未走 read_storage_json: {c:?}");
    let d = find("let storage = read_storage_json(app_kind);");
    assert_eq!(d.len(), 1, "★ discover_app 未走 read_storage_json: {d:?}");
    println!(
        "[wiring] read_entitlement 处 = trae_discover.rs:{}；discover_app 处 = trae_discover.rs:{}",
        c[0], d[0]
    );

    // (d) stats 无生产消费方：with_stats 只被 read_storage_json 与自身定义引用
    let e = find("read_storage_json_with_stats");
    println!("[wiring] 全文件 read_storage_json_with_stats 出现行号 = {e:?}");
    assert_eq!(
        e.len(),
        2,
        "★ 出现次数变了（可能是接线状态变化，请复核）: {e:?}"
    );

    // (e) discover_all → discover_app 链
    let f = find("discover_app(app_kind");
    assert!(!f.is_empty(), "★ discover_all 未调用 discover_app");
    println!("[wiring] discover_all→discover_app 处 = trae_discover.rs:{}", f[0]);

    // (f) ★ 缺陷已修（task-16 复核发现 → task-18 修复 1）：原实现用扁平键取
    //     "icube_gtm.users"，而本机真实 storage.json 里 icube_gtm 是**嵌套**对象
    //     （icube_gtm → users → uid）⇒ 该路证据曾恒为空表。
    //
    //     本断言在 task-16 时钉的是**缺陷**（要求扁平取法恰 1 处）；task-18 改掉
    //     生产代码后它如期变红 —— 那正是当初设计它的目的。现在改为钉住**修复后**
    //     的状态：生产代码里扁平写法必须**绝迹**，嵌套取法必须恰 1 处。
    //     剩下的扁平写法只允许出现在**叙述旧实现**的文档注释里，故按「注释内」计数。
    let g = find(r#"storage.get("icube_gtm.users")"#);
    let code_only: Vec<usize> = g
        .iter()
        .copied()
        .filter(|n| {
            let l = lines[n - 1].trim_start();
            !(l.starts_with("///") || l.starts_with("//!"))
        })
        .collect();
    assert!(
        code_only.is_empty(),
        "★ 生产代码里仍有扁平键取法（真实数据恒取不到）: {code_only:?}"
    );
    assert!(
        !g.is_empty(),
        "★ 文档注释里叙述旧实现的扁平写法不见了 —— 若是有人删掉了这段历史说明请复核"
    );
    println!("[wiring] ★ 扁平键写法仅存于文档注释（叙述旧实现）= trae_discover.rs:{g:?}");

    let nested = find(r#".get("icube_gtm")"#);
    assert_eq!(nested.len(), 1, "★ 嵌套取法应恰 1 处: {nested:?}");
    println!("[wiring] ★ 嵌套取法（已修复）= trae_discover.rs:{}", nested[0]);

    // 反证：扁平取法在真实（嵌套）形态下取不到，嵌套取法取得到
    let nested_val = serde_json::json!({
        // ★ uid 用合成值：真实账号标识不得进入仓库（任务书硬约束）
        "icube_gtm": { "users": { "1883919207380040": { "gtm101_idea": { "showCount": 1 } } } }
    });
    assert!(
        nested_val.get("icube_gtm.users").is_none(),
        "扁平键在嵌套形态下应当取不到"
    );
    assert!(
        nested_val.get("icube_gtm").and_then(|v| v.get("users")).is_some(),
        "嵌套取法应当取得到"
    );
}
// ---------------------------------------------------------------- 8. base64 入口宽容度

/// base64 **入口宽容度** 的跨实现一致性 —— **task-18 已收敛**。
///
/// ## 修复前（task-16 复核发现，本测试当时用来钉住缺陷）
///
/// 参考实现的入口一律是 `Buffer.from(value, 'base64')`，而 Node 的解码器是宽容的：
/// URL-safe 字母表（`-`/`_`）、缺失的 `=` 补齐、混入的非法字符，都能解出**同一份密文**。
/// 当时我方 `decrypt_tc_base64` 与入口判据 `looks_like_encrypted_base64` 都用严格的
/// `general_purpose::STANDARD`。严重性不在于「报错文案不同」，而在于**判据也用同一个
/// 严格解码器**：解码失败即 `false` ⇒ `decrypt_storage_values_with_stats` 里
/// `!looks_like_encrypted_base64(s)` 直接 `continue` ⇒ 该键**既不计入 `encrypted`
/// 也不计入 `failed`**，`has_failures()` 也报不出来 —— 即「上游哪天改用非标准 base64
/// 形态，我方就静默漏账号」，与 `looks_like_tc_base64` 只认 `tc` 头属同一族静默失败。
///
/// ## 修复后（task-18）
///
/// 两边共用**同一条**宽容解码路径 `decode_base64_lenient`（判据与解码同源，
/// 不变式：判据为真 ⇒ 解码必定拿得到字节），并逐条对齐实测的 Node 语义
/// （探针 `test-bin/verify-trae/probe_node_lenient.js`，Node v26.0.0）。
///
/// 本测试现在钉的是**修复本身**，四组断言：
/// (a) 5 条形态参考实现全部接受且解出源明文；
/// (b) 我方判据**全部为真**、解码**全部成功**、明文与参考实现**逐字节相同**；
/// (c) **静默跳过集合必须为空** —— 这正是修复前那条缺陷的反面判据；
/// (d) storage 层端到端：5 条全部计入 `encrypted` + `decrypted`，`failed == 0`
///     （修复前其中 3 条连 `scanned` 之外什么都不计）。
///
/// ★ 唯一有意保留的分歧是非 ASCII 输入（Node 按 Latin-1 截断取低 8 位，我方直接判为
/// 非密文），理由与机器判据见 `trae_crypt.rs` 的 `decode_base64_lenient` 与单测
/// `非_ascii_输入判为非密文`：真实密文是 `Buffer.toString("base64")` 的产物、只含 ASCII。
#[test]
fn base64_entry_leniency_converges_with_node_no_silent_skip() {
    let bx = load_json(BASE64_REL);
    let rows = bx["rows"].as_array().expect("rows 数组");
    let plaintext = bx["_sourceVectorPlaintext"]
        .as_str()
        .expect("_sourceVectorPlaintext");

    let mut js_ok = 0usize;
    let mut rust_ok = 0usize;
    let mut silent_skip: Vec<String> = Vec::new();

    for r in rows {
        let id = r["id"].as_str().expect("id");
        let value = r["value"].as_str().expect("value");
        let expect_rust = r["expectRustAccepts"].as_bool().expect("expectRustAccepts");
        let js_succeeded = r["jsSucceeded"].as_bool().expect("jsSucceeded");
        let js_plain = r["jsPlaintext"].as_str();

        // (a) 参考实现（Node）对这条形态的**真实**行为
        assert!(
            js_succeeded,
            "★ 参考实现本应接受 {id}（生成器实测如此），却报错: {}",
            r["jsError"]
        );
        assert_eq!(
            js_plain,
            Some(plaintext),
            "★ 参考实现解出的明文与源向量不一致: {id}"
        );
        js_ok += 1;

        // (b) 我方入口判据 + 解码入口的**真实**行为
        let judged = looks_like_encrypted_base64(value);
        let decoded = decrypt_tc_base64(value);
        let rust_accepts = decoded.is_ok();

        assert!(
            expect_rust,
            "★ 生成器应已把 {id} 标为「Rust 接受」（task-18 收敛后全表为 true）"
        );
        assert!(
            rust_accepts,
            "★ {id}: 宽容解码后仍失败 —— 收敛被回退了吗？错误: {:?}",
            decoded.as_ref().err().map(variant)
        );
        assert!(
            judged,
            "★ {id}: 能解出却判为「不像密文」⇒ 调用方会静默跳过"
        );
        assert_eq!(
            decoded.as_deref().unwrap(),
            plaintext,
            "★ {id}: 我方解出的明文与参考实现不一致"
        );
        rust_ok += 1;
        println!("[b64] {id:<20} JS=OK Rust=OK 明文逐字节相同（{} B）", plaintext.len());

        // (c) 反向排查：任何一条都不允许落进「静默跳过」这一类
        if !judged && decoded.is_err() {
            silent_skip.push(id.to_string());
        }
    }

    assert_eq!(js_ok, rows.len(), "参考实现应接受全部形态");
    assert_eq!(
        rust_ok,
        rows.len(),
        "★ 我方应接受全部形态（task-18 后 5 条全部收敛）"
    );
    assert!(
        silent_skip.is_empty(),
        "★ 静默跳过集合必须为空（修复前这里至少有 3 条），实际: {silent_skip:?}"
    );

    // (d) storage 层端到端：修复前有 3 条**既不计 encrypted 也不计 failed**
    let mut obj = serde_json::Map::new();
    for r in rows {
        obj.insert(
            r["id"].as_str().expect("id").to_string(),
            Value::String(r["value"].as_str().expect("value").to_string()),
        );
    }
    let (out, stats) = decrypt_storage_values_with_stats(&Value::Object(obj));
    assert_eq!(stats.scanned, rows.len(), "全部形态都应被扫描");
    assert_eq!(
        stats.encrypted,
        rows.len(),
        "★ 全部形态都应被判为密文（修复前只有 1 条）"
    );
    assert_eq!(
        stats.decrypted,
        rows.len(),
        "★ 全部形态都应解密成功（修复前只有 1 条）"
    );
    assert_eq!(stats.failed, 0, "★ 不应有任何失败计数");
    assert!(!stats.has_failures());
    // ★ 注意：解密出的明文本身就是 JSON，`decrypt_storage_values_with_stats` 会把它
    //   再解析成结构化值（`trae_crypt.rs:513`），所以这里比对的是**解析后的 JSON**，
    //   不是字符串。
    let expected: Value = serde_json::from_str(plaintext).expect("源明文应是 JSON");
    for r in rows {
        let id = r["id"].as_str().expect("id");
        assert_eq!(
            out.get(id),
            Some(&expected),
            "★ storage 层 {id} 的明文与参考实现不一致"
        );
    }

    println!(
        "[b64] 共 {} 条形态；参考实现与我方**全部**接受，明文逐字节相同；静默跳过 0 条；\
         storage 层 encrypted={} decrypted={} failed={}",
        rows.len(),
        stats.encrypted,
        stats.decrypted,
        stats.failed
    );
}
// ---------------------------------------------------------------- 9. 两个共享限制

/// 记录两个**参考实现与我方共有**的限制，并给出「今天是否造成真实损失」的机器判据。
///
/// (a) **不递归**：两边的 bulk API 都只看顶层键。参考实现
///     `decryptAllEncryptedValues`（`trae-decrypt.js:176`）遍历 `Object.entries(storage)`
///     不深入；我方 `decrypt_storage_values_with_stats` 对非字符串值原样返回（同样不深入）。
///     ⇒ 这不是分歧，是共同限制。真实影响判据：本机三份真实 `storage.json` 的
///     **嵌套层**里密文命中数 = 0（`gen_base64_variants.js` 同族的探针脚本实测），
///     所以今天无损失；但若上游哪天把 `iCubeAuthInfo://*` 挪进嵌套结构，两边会同时失效。
///
/// (b) **键序**：`serde_json` 在本工作区**未启用 `preserve_order`**（`Cargo.lock` 里
///     `serde_json` 的依赖列表无 `indexmap`），因此 `Value::Object` 是 `BTreeMap`
///     ⇒ 我方输出的键序是**字典序**，而 JS 对象保持插入序。
///     本测试只断言「稳定且可重复」，并把真实序打印出来 —— 因为下游全部用
///     `get(key)` 取值，键序不影响语义；但任何**逐字比对序列化文本**的测试都必须知道这点。
#[test]
fn shared_limitations_non_recursive_and_key_order() {
    // (a) 不递归：嵌套密文对两边都是盲区
    let nested = serde_json::json!({
        "top": "not-a-container",
        "wrapper": {
            "iCubeAuthInfo://icube.cloudide": "dGMFEAAAb38xBENY",
            "deeper": { "x": "dGMFEAAAb38xBENY" }
        }
    });
    let (out, stats) = decrypt_storage_values_with_stats(&nested);
    assert_eq!(
        out["wrapper"], nested["wrapper"],
        "★ 嵌套对象应原样返回（不递归）—— 若这里变了说明实现了递归，请更新本条记录"
    );
    assert_eq!(stats.encrypted, 0, "嵌套里的密文不计入 encrypted");
    assert_eq!(stats.decrypted, 0, "嵌套里的密文不会被解开");
    println!("[limits] (a) 不递归：嵌套密文不计入统计，两边共同限制（今天真实数据命中数=0）");

    // (b) 键序：只断言稳定性 + 打印真实序
    let mut ordered = serde_json::Map::new();
    ordered.insert("z_last".to_string(), Value::from("1"));
    ordered.insert("a_first".to_string(), Value::from("2"));
    ordered.insert("m_mid".to_string(), Value::from("3"));
    let before: Vec<String> = ordered.keys().cloned().collect();
    let (out2, _) = decrypt_storage_values_with_stats(&Value::Object(ordered));
    let after: Vec<String> = out2.as_object().unwrap().keys().cloned().collect();
    assert_eq!(before, after, "★ 键序应当稳定（同一输入两次结果一致）");

    let sorted = {
        let mut v = before.clone();
        v.sort();
        v
    };
    let is_sorted = before == sorted;
    println!(
        "[limits] (b) 键序 = {before:?}；字典序 = {is_sorted}（serde_json 未启用 preserve_order ⇒ 应为 true）"
    );
    println!("[limits]     下游全部用 get(key) 取值 ⇒ 键序不影响语义，但禁止逐字比对序列化文本");
}
