//! Qoder 请求体编码（QoderEncoding）。
//!
//! 算法三步（缺一不可，来自参考实现 `docs/api-reference.md` §5）：
//!
//! ① 标准 base64
//! ② 三段重排：把末尾 1/3 移到最前、开头 1/3 移到最后
//! ③ 字符映射：标准字母表 → 自定义字母表，`'='` → `'$'`
//!
//! # 为什么必须编码
//!
//! 上游的对话端点在 URL 里带 `Encode=1`。实测与参考实现均表明：
//! 请求体需要按此格式编码后发送，否则上游无法解析（表现为流挂起或业务错误）。
//!
//! # 为什么两个方向都要实现
//!
//! [`qoder_encode`] 用于请求体；[`qoder_decode`] 用于**验证往返正确性**
//! （单测里 encode→decode 必须还原原文，否则算法有笔误）。
//! 另外若上游哪天改为编码响应，decode 也已就绪。
//!
//! ⚠ 自定义字母表里有 `,` `@` `#` `&` `*` `%` `^` `.` `(` `)` `!` 等字符 ——
//! 它们不是 base64 合法字符，故这份编码结果**不能**当 base64 用。
//! 复制粘贴字母表时极易漏字符或改顺序，改错会让所有请求静默失败，
//! 故字母表本身也有单测钉住（见本文件末尾）。

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;

/// 上游的自定义字母表（64 字符，与标准 base64 逐位对应）。
pub(crate) const CUSTOM_ALPHABET: &str =
    "_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!";

/// 标准 base64 字母表。
pub(crate) const STD_ALPHABET: &str =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 自定义填充字符（替代 `'='`）。
pub(crate) const CUSTOM_PAD: char = '$';

/// 把标准 base64 字母表映射到自定义字母表。
///
/// 用 `[u8; 128]` 而不是 `HashMap`：与 Go 侧 `[128]byte` 同构，
/// 且查询在热路径上（每个请求体逐字符过一遍）。
pub(crate) fn std_to_custom_table() -> [u8; 128] {
    let mut t = [0xFFu8; 128];
    let std = STD_ALPHABET.as_bytes();
    let custom = CUSTOM_ALPHABET.as_bytes();
    debug_assert_eq!(std.len(), 64);
    debug_assert_eq!(custom.len(), 64);
    for i in 0..64 {
        t[std[i] as usize] = custom[i];
    }
    t
}

/// 把自定义字母表映射回标准字母表；`'$'` 映射回 `'='`。
///
/// 仅供 [`qoder_decode`] 与往返自检使用，生产路径（[`qoder_encode`]）用不到。
#[allow(dead_code)]
pub(crate) fn custom_to_std_table() -> [u8; 256] {
    let mut t = [0xFFu8; 256];
    let std = STD_ALPHABET.as_bytes();
    let custom = CUSTOM_ALPHABET.as_bytes();
    for i in 0..64 {
        t[custom[i] as usize] = std[i];
    }
    t[CUSTOM_PAD as usize] = b'=';
    t
}

/// 编码请求体。
///
/// 与 Go 侧 `QoderEncode` 逐字节等价。
pub(crate) fn qoder_encode(plain: &[u8]) -> String {
    let std = B64.encode(plain);
    let n = std.len();
    if n == 0 {
        return String::new();
    }
    let std_bytes = std.as_bytes();

    // ② 三段重排：末尾 1/3 + 中间 1/3 + 开头 1/3
    let a = n / 3;
    let mut rearranged = Vec::with_capacity(n);
    rearranged.extend_from_slice(&std_bytes[n - a..]);
    rearranged.extend_from_slice(&std_bytes[a..n - a]);
    rearranged.extend_from_slice(&std_bytes[..a]);

    // ③ 字符映射
    let table = std_to_custom_table();
    let mut out = String::with_capacity(n);
    for &c in &rearranged {
        if c == b'=' {
            out.push(CUSTOM_PAD);
            continue;
        }
        let mapped = table[c as usize];
        if mapped == 0xFF {
            // 理论上不可达（base64 输出只含标准字母表），
            // 但保留分支以免将来改字母表时静默产出坏数据。
            panic!("qoder: base64 输出含意外字符 {:?}", c as char);
        }
        out.push(mapped as char);
    }
    out
}

/// 逆编码。主要用于往返自检与响应解码。
///
/// Go 侧 `QoderDecode` 同样是**导出但生产不调用**的（只在 `encoding_test.go`
/// 与 `userstatus_test.go` 里用）—— 生产路径只编码不解码，留着是为了让
/// "编码表改错了"能被往返测试立刻抓到。
#[allow(dead_code)]
pub(crate) fn qoder_decode(enc: &str) -> Result<Vec<u8>, String> {
    let bytes = enc.as_bytes();
    let n = bytes.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    // ③ 逆映射
    let table = custom_to_std_table();
    let mut mapped = Vec::with_capacity(n);
    for (i, &c) in bytes.iter().enumerate() {
        let s = table[c as usize];
        if s == 0xFF {
            return Err(format!("非法字符 {:?}（位置 {}）", c as char, i));
        }
        mapped.push(s);
    }
    // ② 逆重排
    let a = n / 3;
    let mut std = Vec::with_capacity(n);
    std.extend_from_slice(&mapped[n - a..]);
    std.extend_from_slice(&mapped[a..n - a]);
    std.extend_from_slice(&mapped[..a]);
    // ① base64 解码
    let s = String::from_utf8(std).map_err(|e| format!("编码结果不是合法 UTF-8: {e}"))?;
    B64.decode(s.as_bytes())
        .map_err(|e| format!("base64 解码失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alphabet_is_exactly_64_chars() {
        // 字母表写错会让**每个**请求静默失败，现象是「账号用不了」，
        // 排查方向会被引向凭证。钉死它。
        assert_eq!(CUSTOM_ALPHABET.chars().count(), 64);
        assert_eq!(STD_ALPHABET.chars().count(), 64);
        // 逐字符打印以便人工核对（失败时能看到实际内容）
        assert_eq!(
            CUSTOM_ALPHABET,
            "_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!"
        );
    }

    #[test]
    fn round_trip_restores_original() {
        for case in [
            &b""[..],
            b"a",
            b"ab",
            b"abc",
            b"abcd",
            b"hello world",
            "中文内容也要能往返".as_bytes(),
            &[0u8, 1, 2, 253, 254, 255][..],
        ] {
            let enc = qoder_encode(case);
            let dec = qoder_decode(&enc).expect("decode 应当成功");
            assert_eq!(dec, case, "往返失败: {:?}", case);
        }
    }

    #[test]
    fn encode_matches_go_semantics_for_known_input() {
        // "{}" 的标准 base64 是 "e30="，长度 4，a = 4/3 = 1
        // 重排：std[3..] + std[1..3] + std[..1] = "=" + "30" + "e" = "=30e"
        // 映射：'=' → '$'，'3'→自定义[54]，'0'→自定义[52]，'e'→自定义[30]
        let enc = qoder_encode(b"{}");
        assert_eq!(enc.len(), 4);
        assert!(enc.starts_with(CUSTOM_PAD), "首位应当是自定义填充: {enc}");
        assert_eq!(qoder_decode(&enc).unwrap(), b"{}");
    }

    #[test]
    fn decode_rejects_illegal_char() {
        // ⚠ 字母表里**一个数字都没有**（这是它的特征之一），故 '0' 必然非法。
        //   （最初这里写的是 'A' —— 而 'A' 恰好是字母表的最后一个字符，测试
        //   因为假设错了而失败，不是实现错了。）
        assert!(!CUSTOM_ALPHABET.contains('0'));
        let err = qoder_decode("0").unwrap_err();
        assert!(err.contains("非法字符"), "错误文案应指出非法字符: {err}");
        // 位置也要报对（多字符时不能只报第一个）
        let err2 = qoder_decode("_0_").unwrap_err();
        assert!(err2.contains("位置 1"), "应报出出错位置: {err2}");
    }

    #[test]
    fn tables_are_bijective() {
        let fwd = std_to_custom_table();
        let back = custom_to_std_table();
        for &c in STD_ALPHABET.as_bytes() {
            let m = fwd[c as usize];
            assert_ne!(m, 0xFF);
            assert_eq!(back[m as usize], c, "映射不可逆: {}", c as char);
        }
        assert_eq!(back[CUSTOM_PAD as usize], b'=');
        assert_eq!(fwd[b'=' as usize], 0xFF, "标准 '=' 不在标准字母表内");
    }

    #[test]
    fn encode_empty_is_empty() {
        assert_eq!(qoder_encode(b""), "");
        assert_eq!(qoder_decode("").unwrap(), Vec::<u8>::new());
    }
}
