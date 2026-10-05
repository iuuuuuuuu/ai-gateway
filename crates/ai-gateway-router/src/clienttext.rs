//! clienttext.rs 面向客户端的文案闸门：**不让裸 401/403 出现在任何对外文案里**。
//!
//! 逐字移植 Go 侧 `internal/server/clienttext.go`（54 行）与 `forward.go:1038-1096`
//! 的 `clientFacingStatus`，判据与文案一字不改。
//!
//! # 要解决的问题（2026-09-20 实测缺陷，所有者报告）
//!
//! 所有者原话：
//!
//! > 「密钥明明是正确的，不知道怎么对话过程中就提示『本轮运行失败 API 密钥无效』，
//! > 而且我还是可以通过我配置的密钥获取到模型的名称」
//!
//! 查证结论：**密钥从头到尾都是对的**，是错误信息被客户端误读了。
//!
//! # ★ 真正的判据是**文案正则**，不是 HTTP 状态码（2026-09-29 更正）
//!
//! 本模块的作者原先以为客户端是按状态码判的：
//!
//! ```text
//! if (status === 401 || status === 403) return "AUTH";   // ← 那条路径不生效
//! ```
//!
//! DSH 里 `ai` provider 的 namespace 是 `llm-pi-ai`，走的是 pi-ai 适配层。
//! 真正的判据是**文案文本正则**（`dsh-llm-pi-ai/lib/index.js:1376-1387`）：
//!
//! ```text
//! function classifyPiAiError(message) {
//!   if (/\b(?:401|403)\b/.test(message)) return "AUTH";
//!   ...
//! }
//! ```
//!
//! 而错误文案来自上游响应体：`pi-ai/dist/utils/error-body.js:111-118` 的
//! `formatProviderError` 会把 `(status): body` **原样拼进 message**。
//!
//! ⇒ **只要文案里出现裸 "401"/"403"，不管 HTTP 状态码是什么，都被判 AUTH。**
//!
//! 这就是为什么**只把 403 改写成 502 是无效的** —— 上一版正是这么做的，
//! 所有者装了新包实测仍然报「API 密钥无效」，因为文案里还留着
//! 「上游 403 业务码 11140」这几个字。**光改状态码治不了这个病。**
//!
//! 所以有**两道**防线，缺一不可：
//!
//! 1. 状态码：[`client_facing_status`] 把 403 → 502，避免任何按状态码判的分支误读；
//! 2. 文案：[`client_safe_message`] 在**所有**对外出口把裸 401/403 就地改写成 4xx
//!    —— 这才是真正兜住 DSH 的那道。
//!
//! # 为什么状态码改成 502 而不是保留 403
//!
//! 语义上 403 是「服务器理解请求但拒绝执行」，属于**上游**的决定；而 401 才是
//! 「你没带对凭据」。403 被客户端当成凭据问题纯属误读。502 Bad Gateway 准确
//! 表达「上游拒绝了 / 不可用」，且客户端不会把它归类成 AUTH，而是走 `SERVER`
//! 分支 —— 那才是可重试的正确语义。
//!
//! ⚠ **我们自己从不回 403**（鉴权中间件只用 401），故这里把 403 改掉不会与
//! 「我们自己的拒绝」混淆。
//!
//! # 为什么 401 必须保持 401
//!
//! 那是我们自己的鉴权结果，**必须**保持 401 让客户端提示「密钥无效」——
//! 那种情况下提示是对的。注意它仍然要过 [`client_safe_message`]：我们自己的
//! 401 文案是 `"missing or invalid API key"`（不含裸数字），改写是恒等变换。

/// 把文案里的**裸 401/403** 就地改写成 `4xx`（幂等）。
///
/// 「裸」= 前后都不是**单词字符**（`[0-9A-Za-z_]`）。这一条是本模块的全部精度来源：
///
/// * 四位业务码 `11140` / `14018` / `11133` / `11102` / `11115` / `10605` /
///   `6004` / `3007` **逐字不变**；
/// * ★ 尤其 `14018` —— 它的中间三位**正好是 `401`**，靠词边界保护
///   （`14018` 里 `401` 左侧是 `1`，是单词字符 ⇒ 无边界 ⇒ 不动它）。
///
/// # ⚠ 为什么不用 `regex` crate 的 `\b`（Rust 与 Go 在这里**语义不同**）
///
/// Go 的 `\b` 按 **ASCII** 定义单词字符（`\w = [0-9A-Za-z_]`），而 Rust `regex`
/// crate 的 `\b` **默认是 Unicode 感知**的 —— 汉字算单词字符。两者可观测地不同：
///
/// ```text
/// "上游403业务码"
///   Go   ：游 / 业 都不是 ASCII 单词字符 ⇒ 有边界 ⇒ 命中 ⇒ 改写成 "上游4xx业务码"
///   Rust ：游 / 业 都是 Unicode 单词字符 ⇒ 无边界 ⇒ 漏判（裸 403 留在文案里）
/// ```
///
/// 而「紧贴中文的裸 403」正是真实上游文案的常见形状（Go 侧注释也专门点了这一条）。
/// 漏判的后果就是本模块要修的那个缺陷原样复发 —— 用户又看到「API 密钥无效」。
///
/// 故这里**手写字节级扫描**，单词字符定义与 Go 的 `\b` 逐字一致（只认 ASCII）。
/// 若要改用 `regex` crate，必须写成 `(?-u:\b(?:401|403)\b)` 才是同一语义 ——
/// 但那样正确性就依赖于读者记得那个 `(?-u:...)`，不如显式实现来得可靠。
///
/// # 幂等
///
/// 替换结果是 `4xx`，其中不含 `401`/`403`，故 `f(f(x)) == f(x)`。
/// 这一点是硬要求：网关有多层出口（`handler.rs` / `messages.rs` / `responses.rs` /
/// `responses_stream.rs`），同一段文案可能被过两次。
pub fn client_safe_message(msg: &str) -> String {
    let bytes = msg.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    let mut rewritten = 0usize;
    while i < bytes.len() {
        if is_bare_auth_code(bytes, i) {
            // Go：`s[:1] + "xx"`（`s` 是命中的 "401"/"403"）⇒ 只改中间两位数字。
            out.extend_from_slice(b"4xx");
            rewritten += 1;
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    if rewritten > 0 {
        // 与 Go 一样在此处报警：出现裸 401/403 说明**某处文案自己带上了状态码**，
        // 应当去修那个产出点，而不是长期依赖这道闸门兜着。
        // （Go 侧是 `log.Printf`，本 crate 既有代码统一用 `eprintln!`。）
        eprintln!(
            "clienttext: 文案里出现裸 401/403，已就地改写为 4xx（请修正该文案的产出点）：{msg:?}"
        );
    }
    // 只做 ASCII 替换、其余字节逐字复制 ⇒ 结果必然仍是合法 UTF-8。
    // 用 lossy 兜底而非 `unwrap`：这是**面向客户端的出口**，
    // 任何情况下都必须能返回一段文案，绝不能在这里 panic 掉整个请求。
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// 上游状态码 → 面向客户端的状态码（403 → 502，其余原样）。
///
/// 401 **不在这里处理**（见模块头）：那是我们自己的鉴权结果。
///
/// ⚠ 任何输入都**不会**返回 403 —— 本函数的存在意义就是消灭 403。
pub fn client_facing_status(status: u16) -> u16 {
    if status == 403 {
        // 502 Bad Gateway：准确表达「上游拒绝了 / 不可用」，
        // 客户端据此走 SERVER 分支（可重试），而不是 AUTH（提示密钥无效）。
        502
    } else {
        status
    }
}

/// `bytes[i..]` 处是否是**裸的** `401` / `403`。
///
/// 前置条件由调用方保证：`i < bytes.len()`。
fn is_bare_auth_code(bytes: &[u8], i: usize) -> bool {
    if i + 3 > bytes.len() {
        return false;
    }
    let code = &bytes[i..i + 3];
    if code != b"401" && code != b"403" {
        return false;
    }
    // Go 的 `\b`：两侧都不是 ASCII 单词字符才算边界。
    !is_word_byte(byte_at(bytes, i as isize - 1)) && !is_word_byte(byte_at(bytes, (i + 3) as isize))
}

/// 取 `s[i]`，越界返回 `0`（调用方用它表示「没有相邻字符」= 边界）。
///
/// 与 `wafip.rs` 里的同名私有函数逐字同义。两处刻意各留一份而不是共用：
/// 它们是**两个独立模块**各自的判据基础，共用一个 `pub(crate)` 助手会让
/// 「改一处影响另一处」，而这两个模块要分别对照各自的 Go 源做逐字核对。
fn byte_at(s: &[u8], i: isize) -> u8 {
    if i < 0 || i as usize >= s.len() {
        return 0;
    }
    s[i as usize]
}

/// 报告 `b` 是否为「ASCII 单词字符」（字母 / 数字 / 下划线）——
/// 与 Go `\b` 所用的 `\w` 逐字一致。
///
/// ⚠ 非 ASCII 一律返回 `false`（= 视为边界）。这个方向是刻意的：
///
/// * 中文等文字**不用空格分词**，`"上游403业务码"` 里的 403 必须能被改写 ——
///   把汉字当单词字符会让它漏判，而漏判 = 缺陷复发（用户又看到「密钥无效」）；
/// * 而需要防的误判（`14018` / `11140` / `4012`）其相邻字符必然是 ASCII
///   字母或数字，与本分支无关。
///
/// 两个方向的收益不对称：漏判让修复失效，误判只是多改写一个数字串
/// （`4xx` 同样不含裸 401/403，客户端仍走 SERVER 分支，不会更糟）。
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ① 裸 401/403 被就地改写成 4xx。
    #[test]
    fn rewrites_bare_auth_codes() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "上游状态码（Go forward.go 拼进文案的形状）",
                "upstream error (http 403): blocked",
                "upstream error (http 4xx): blocked",
            ),
            (
                "裸 401",
                "upstream returned 401 for this key",
                "upstream returned 4xx for this key",
            ),
            (
                "中文空格分隔",
                "上游 403 业务码 11140",
                "上游 4xx 业务码 11140",
            ),
            (
                "★ 紧贴中文（Go 的 ASCII \\b 会命中，Rust 的 Unicode \\b 会漏判）",
                "上游403业务码",
                "上游4xx业务码",
            ),
            (
                "★ 紧贴中文的另一侧",
                "403被上游拒绝",
                "4xx被上游拒绝",
            ),
            (
                "多处命中全部改写",
                "403 and 401 and 403",
                "4xx and 4xx and 4xx",
            ),
            (
                "JSON 片段里的裸值",
                r#"{"code":403}"#,
                r#"{"code":4xx}"#,
            ),
            (
                "括号 / 句号 / 冒号等非单词字符都算边界",
                "[403].401:403",
                "[4xx].4xx:4xx",
            ),
            (
                "行首行尾（越界 = 边界）",
                "401",
                "4xx",
            ),
        ];
        for (name, input, want) in cases {
            assert_eq!(
                client_safe_message(input),
                *want,
                "{name}：client_safe_message({input:?})"
            );
        }
    }

    /// ② ★ 业务码**逐字不变**（这是本模块精度的全部意义）。
    ///
    /// 尤其 `14018`：它的中间三位正好是 `401`，靠词边界保护。
    #[test]
    fn business_codes_untouched() {
        // 上游实际会返回的业务码（见 Go 侧 clienttext_test.go 的清单）。
        let codes = [
            "11140", "14018", "11133", "11102", "11115", "10605", "6004", "3007",
        ];
        for code in codes {
            // 逐个位置嵌入，确保任何上下文中都不被改写。
            let contexts = [
                format!("上游业务码 {code}"),
                format!(r#"{{"code":{code},"msg":"x"}}"#),
                format!(r#"{{"code":"{code}","msg":"x"}}"#),
                format!("error {code}"),
                code.to_string(),
                format!("上游业务码{code}"),
            ];
            for ctx in &contexts {
                assert_eq!(
                    client_safe_message(ctx),
                    *ctx,
                    "业务码 {code} 在上下文 {ctx:?} 中被改写了 —— 业务码必须逐字不变"
                );
            }
        }
    }

    /// ②b ★★ 专门钉 `14018` 的 `401` 子串（Lead 明确要求单列一条）。
    ///
    /// `14018` 的字节是 `1 4 0 1 8`；`401` 出现在下标 1，其左侧是 `1`（单词字符）
    /// ⇒ 无词边界 ⇒ **不得**改写。若实现里用了裸 `contains("401")`，这里就会
    /// 变成 `14xx8`，把上游的业务码改坏，用户再也查不到真实错误。
    #[test]
    fn code_14018_contains_401_but_must_not_be_rewritten() {
        assert!(
            "14018".contains("401"),
            "前置事实：14018 的中间三位确实是 401（这条测试的存在理由）"
        );
        for ctx in [
            "14018",
            "上游 14018",
            r#"{"code":14018}"#,
            "request failed: code=14018",
            "上游业务码14018，请检查套餐",
        ] {
            assert_eq!(
                client_safe_message(ctx),
                ctx,
                "14018 被改写成 {:?} —— 业务码里的 401 子串必须靠词边界保护",
                client_safe_message(ctx)
            );
        }
        // 同时确认：真正的裸 401 在同一句里仍要被改写。
        assert_eq!(
            client_safe_message("上游 14018，但网关回了 401"),
            "上游 14018，但网关回了 4xx",
            "同句里业务码保留、裸状态码改写"
        );
    }

    /// ③ 429 / 5xx / 400 / 413 原样保留（那是可重试语义）。
    ///
    /// 这些数字**不是** AUTH 判据（正则只认 401/403），改掉它们只会让用户
    /// 失去「该不该重试」的信息。
    #[test]
    fn retryable_codes_preserved() {
        let codes = ["429", "400", "413", "500", "502", "503", "504", "501", "599"];
        for code in codes {
            for ctx in [
                format!("upstream error (http {code})"),
                format!("上游 {code}"),
                format!(r#"{{"code":{code}}}"#),
                code.to_string(),
            ] {
                assert_eq!(
                    client_safe_message(&ctx),
                    ctx,
                    "{code} 被改写了 —— 429/5xx/400/413 必须原样保留（可重试语义）"
                );
            }
        }
    }

    /// ④ 幂等：`f(f(x)) == f(x)`。
    ///
    /// 硬要求：网关有多层出口（handler / messages / responses /
    /// responses_stream），同一段文案可能被过两次。
    #[test]
    fn idempotent() {
        let corpus = [
            "upstream error (http 403): blocked",
            "上游 403 业务码 11140",
            "上游403业务码",
            "14018",
            r#"{"code":14018,"msg":"403"}"#,
            "missing or invalid API key",
            "",
            "403",
            "403403403",
            "上游 401 / 403 都被改写",
        ];
        for input in corpus {
            let once = client_safe_message(input);
            let twice = client_safe_message(&once);
            assert_eq!(
                once, twice,
                "client_safe_message 不幂等：{input:?} → {once:?} → {twice:?}"
            );
        }
    }

    /// 我们自己那条 401 文案必须是恒等变换。
    ///
    /// 它是「密钥无效」提示的正主：**不含裸数字**，故闸门不该动它
    /// （若这里被改写，说明词边界判错了，或者文案里混进了裸状态码）。
    #[test]
    fn own_auth_message_is_identity() {
        let msg = "missing or invalid API key";
        assert_eq!(client_safe_message(msg), msg);
    }

    /// 不含任何状态码的普通文案必须是恒等变换（闸门不该有副作用）。
    #[test]
    fn plain_messages_are_identity() {
        let corpus = [
            "no healthy account available",
            "请求上下文超出模型窗口",
            "上游风控（3012 unusual activity）：与账号无关",
            "上游疑似 WAF 拦截：最近 60 秒内已有 2 个不同账号被上游拒绝",
            "账号池已耗尽，请稍后重试",
        ];
        for msg in corpus {
            assert_eq!(client_safe_message(msg), msg, "{msg:?} 不该被改写");
        }
    }

    /// ⑤ `client_facing_status`：403 → 502，401 保持 401。
    #[test]
    fn facing_status_maps_403_to_502() {
        assert_eq!(client_facing_status(403), 502, "403 必须映射成 502");
        assert_eq!(
            client_facing_status(401),
            401,
            "401 必须保持 401 —— 那是我们自己的鉴权结果，提示「密钥无效」是对的"
        );
    }

    /// ⑤b ★ **任何**输入都不得返回 403。
    ///
    /// 这是本函数的存在意义。用穷举（0..=600）钉住，而不是只测几个点 ——
    /// 将来若有人加一条「某业务码映射回 403」的分支，这条会立刻红。
    #[test]
    fn facing_status_never_returns_403() {
        for status in 0u16..=600 {
            assert_ne!(
                client_facing_status(status),
                403,
                "client_facing_status({status}) 返回了 403 —— 本函数存在的意义就是消灭 403"
            );
        }
        // 其余值一律原样（不做别的映射）。
        for status in [0u16, 200, 201, 400, 401, 402, 404, 429, 500, 502, 503, 504, 599, 65535] {
            assert_eq!(
                client_facing_status(status),
                status,
                "只有 403 需要映射，其余状态码必须原样返回"
            );
        }
    }

    /// 两道防线合起来的效果：上游 403 + 文案里带 403
    /// ⇒ 状态码是 502、文案里没有裸 403。
    ///
    /// 这是缺陷修复的**端到端断言**（单模块内能表达的部分）：
    /// 只做其中一道都不够 —— 上一版只改了状态码，所有者装了新包实测仍然报
    /// 「API 密钥无效」，因为文案里还留着「上游 403 业务码 11140」。
    #[test]
    fn both_lines_of_defence_together() {
        let upstream_body = r#"{"code":11140,"msg":"upstream returned 403"}"#;
        let raw = format!("upstream zcode (http 403): {upstream_body}");

        let status = client_facing_status(403);
        let msg = client_safe_message(&raw);

        assert_eq!(status, 502, "第一道防线：状态码 403 → 502");
        assert!(
            !msg.contains("403"),
            "第二道防线：文案里不得再有裸 403，实际 {msg:?}"
        );
        assert!(
            msg.contains("11140"),
            "业务码 11140 必须逐字保留（排查依据），实际 {msg:?}"
        );
        assert!(msg.contains("4xx"), "裸 403 应被改写成 4xx，实际 {msg:?}");
        // 客户端判据（/\b(?:401|403)\b/）在最终文案上必须不再命中。
        assert!(
            !msg.contains("401") && !msg.contains("403"),
            "最终文案仍会被客户端正则判成 AUTH：{msg:?}"
        );
    }

    /// 词边界的逐个边界值（ASCII 单词字符 = `[0-9A-Za-z_]`）。
    #[test]
    fn word_boundary_details() {
        let cases: &[(&str, &str)] = &[
            // 相邻是 ASCII 单词字符 ⇒ 不是裸码 ⇒ 不动
            ("1401", "1401"),
            ("4012", "4012"),
            ("4010", "4010"),
            ("x401", "x401"),
            ("401x", "401x"),
            ("_401", "_401"),
            ("401_", "401_"),
            ("a403b", "a403b"),
            ("1403", "1403"),
            ("4031", "4031"),
            ("abc401def", "abc401def"),
            // 相邻是非单词字符 / 越界 ⇒ 裸码 ⇒ 改写
            ("-401-", "-4xx-"),
            ("(401)", "(4xx)"),
            ("'401'", "'4xx'"),
            ("\"401\"", "\"4xx\""),
            ("401.", "4xx."),
            (".401", ".4xx"),
            ("\t401\n", "\t4xx\n"),
            // 中文两侧（非 ASCII 字节一律算边界）
            ("中401文", "中4xx文"),
            ("、401、", "、4xx、"),
            ("，401，", "，4xx，"),
            // 全角数字不是 ASCII 数字 ⇒ 也算边界（Go 同样：`\w` 只认 ASCII）
            ("４401", "４4xx"),
        ];
        for (input, want) in cases {
            assert_eq!(
                client_safe_message(input),
                *want,
                "client_safe_message({input:?}) 期望 {want:?}"
            );
        }
    }

    /// 非 ASCII 文案不被破坏（字节级复制不得切坏多字节字符）。
    #[test]
    fn multibyte_text_survives() {
        let msg = "上游业务码 11140：账号【测试号】的密钥被拒绝，403 是上游的决定 🚫";
        let out = client_safe_message(msg);
        assert!(out.contains("测试号"), "多字节字符必须完整保留，实际 {out:?}");
        assert!(out.contains("🚫"), "emoji（4 字节 UTF-8）必须完整保留，实际 {out:?}");
        assert!(out.contains("11140"), "业务码保留，实际 {out:?}");
        assert!(out.contains("4xx"), "裸 403 改写，实际 {out:?}");
        assert!(!out.contains("403"), "不该再有裸 403，实际 {out:?}");
    }
}
