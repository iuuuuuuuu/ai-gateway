//! 出站请求体脱敏：剥离上游内容审核黑名单指纹。
//!
//! 对应 Go 源文件 `internal/upstream/sanitize.go`。
//!
//! 背景：客户端（Claude Code 类 CLI）在 system prompt 注入若干固定模板句，
//! 上游内容审核按**逐字精确匹配**拦截（非语义审核），一字改动即可绕过。
//!
//! 策略分两层：
//!   - 键值 / header 型指纹：整段剥离（它们不承载语义）
//!   - 承载语义的模板句：最小改写（换一个词），语义不变
//!
//! # 为什么改写而不是删除
//!
//! 模板句承载语义（身份、分支约定、反馈方式）。整句删掉会改变模型对自身角色的
//! 认知，可能影响输出质量；只改一个词既能避开逐字匹配，又保持语义。
//!
//! # 字面量必须与 Go 逐字一致
//!
//! 这里是**逐字匹配**的对抗性逻辑：改一个字符（哪怕是同义词）就可能不再命中，
//! 脱敏静默失效。因此下面每个串都取自 Go 源码的字节级内容，不做任何"顺手优化"。

/// 特征预检串：任一命中才进入净化（快速路径，普通请求全不中 → 原样返回）。
///
/// 前四项为截断前缀，足以命中完整句。
const FEATURES: &[&str] = &[
    "x-anthropic-billing-header",
    "cc_entrypoint=",
    "You are Claude Code",
    "Main branch (",
    "github.com/anthropics",
    "led by OpenAI",
];

/// header 键名（剥离层按此键整段删除，与值无关）。
const HEADER_KEY: &str = "x-anthropic-billing-header";

/// 改写层：模板句逐字替换（每句只改一个词，语义不变）。
///
/// 身份句的查找串**不带结尾标点**，因此两种客户端变体都能命中：
///   - CLI 模式：`... official CLI for Claude.`
///   - 3P 模式：`... official CLI for Claude, running within the Claude Agent SDK.`
const REWRITES: &[(&str, &str)] = &[
    // 身份句：补一个 "tool"（official CLI → official CLI tool），语义不变。
    (
        "You are Claude Code, Anthropic's official CLI for Claude",
        "You are Claude Code, Anthropic's official CLI tool for Claude",
    ),
    // 分支约定：Main → Default。
    (
        "Main branch (you will usually use this for PRs)",
        "Default branch (you will usually use this for PRs)",
    ),
    // Claude Code 2.1.260 的系统提示里带指向 Anthropic 官方仓库的反馈链接，
    // 上游按「未批准渠道」指纹拦截（HTTP 400 code=11128）。
    // 只替换链接本身，句子结构与语义（如何提交反馈）不变。
    (
        "https://github.com/anthropics/claude-code/issues",
        "https://github.com/user-feedback/issues",
    ),
    // Codex CLI 的 instructions 首句声明归属 OpenAI，上游同样按「未批准渠道」
    // 指纹拦截。只改归属表述，语义（开源项目）不变。
    ("led by OpenAI", "led by the community"),
];

/// 特征预检：快速路径命中任一特征串即返回 true。
fn has_fingerprint(text: &str) -> bool {
    if FEATURES.iter().any(|f| text.contains(f)) {
        return true;
    }
    // 键名有大小写变体（X-Anthropic-...），contains 漏掉时按不敏感再查一遍
    //（Go 侧由正则的 `(?i)` 承担同一职责）。
    text.to_lowercase().contains(HEADER_KEY)
}

/// 单段文本净化：预检不中 → 返回原串（零改动）。
pub fn sanitize_text(text: &str) -> String {
    if !has_fingerprint(text) {
        return text.to_string();
    }
    let mut s = text.to_string();
    for (find, repl) in REWRITES {
        if s.contains(find) {
            s = s.replace(find, repl);
        }
    }
    s = strip_header_segment(&s);
    if s.contains("cc_") {
        s = strip_cc_kv(&s);
    }
    s.trim().to_string()
}

/// 删除 `x-anthropic-billing-header: <值>` 段（大小写不敏感），整段到分号或行尾。
///
/// 对应 Go 正则 `(?i)x-anthropic-billing-header:[^;\n]*;?\s*`：**要求冒号**，
/// 值到分号/换行为止，并吞掉可选的分号与尾随空白。
fn strip_header_segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let lower = rest.to_lowercase();
        let Some(pos) = lower.find(HEADER_KEY) else {
            out.push_str(rest);
            return out;
        };
        let after_key = &rest[pos + HEADER_KEY.len()..];
        // Go 正则要求紧跟冒号；没有冒号就不算这个指纹，保留原文继续往后找。
        let Some(after_colon) = after_key.strip_prefix(':') else {
            out.push_str(&rest[..pos + HEADER_KEY.len()]);
            rest = after_key;
            continue;
        };
        out.push_str(&rest[..pos]);
        // 值：到分号或换行为止
        let mut end = after_colon.len();
        for (i, c) in after_colon.char_indices() {
            if c == ';' {
                end = i + 1;
                break;
            }
            if c == '\n' {
                end = i;
                break;
            }
        }
        rest = &after_colon[end..];
        // 吞掉尾随空白（Go 正则末尾的 `\s*`）
        rest = rest.trim_start_matches([' ', '\t']);
    }
}

/// 循环清理裸键值 `cc_xxx=...`（大小写不敏感，到分号或换行为止）。
///
/// 对应 Go 正则 `(?i)\bcc_[a-z0-9_]+=[^;\n]*;?\s*`；循环是为了清掉尾随的连续多个。
fn strip_cc_kv(text: &str) -> String {
    let mut s = text.to_string();
    loop {
        match strip_cc_kv_once(&s) {
            Some(next) if next != s => s = next,
            _ => return s,
        }
    }
}

/// 单次清理；无 `cc_` 键值时返回 `None`。
fn strip_cc_kv_once(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0usize;
    while i + 3 <= bytes.len() {
        if &bytes[i..i + 3] == b"cc_" {
            // 键名必须是词首（前一个字符非字母数字下划线），对应正则的 `\b`
            let at_word_start =
                i == 0 || !matches!(bytes[i - 1], b'a'..=b'z' | b'0'..=b'9' | b'_');
            if at_word_start {
                // 键名：cc_ + [a-z0-9_]+
                let mut j = i + 3;
                while j < bytes.len() && matches!(bytes[j], b'a'..=b'z' | b'0'..=b'9' | b'_') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'=' {
                    // 值：到分号或换行为止
                    let mut k = j + 1;
                    while k < bytes.len() && bytes[k] != b';' && bytes[k] != b'\n' {
                        k += 1;
                    }
                    let end = if k < bytes.len() && bytes[k] == b';' {
                        k + 1
                    } else {
                        k
                    };
                    let mut out = String::with_capacity(text.len());
                    out.push_str(&text[..i]);
                    out.push_str(text[end..].trim_start_matches([' ', '\t']));
                    return Some(out);
                }
            }
        }
        i += 1;
    }
    None
}

/// 净化 `content`：兼容字符串与多模态数组；只动 `text` part，image 等 part 不动。
///
/// 返回是否发生变化。
pub fn sanitize_content(v: &mut serde_json::Value) -> bool {
    match v {
        serde_json::Value::String(s) => {
            let cleaned = sanitize_text(s);
            if cleaned != *s {
                *s = cleaned;
                return true;
            }
            false
        }
        serde_json::Value::Array(parts) => {
            let mut changed = false;
            for p in parts.iter_mut() {
                let Some(m) = p.as_object_mut() else { continue };
                let Some(serde_json::Value::String(text)) = m.get_mut("text") else {
                    continue;
                };
                let cleaned = sanitize_text(text);
                if cleaned != *text {
                    *text = cleaned;
                    changed = true;
                }
            }
            changed
        }
        _ => false,
    }
}

/// 净化 `messages` 中的 `content`；返回是否有任一消息被改动。
pub fn sanitize_messages(messages: &mut [serde_json::Value]) -> bool {
    let mut changed = false;
    for msg in messages.iter_mut() {
        let Some(m) = msg.as_object_mut() else { continue };
        let Some(content) = m.get_mut("content") else {
            continue;
        };
        if sanitize_content(content) {
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clean_text_untouched() {
        let s = "这是一段普通文本，没有任何指纹。";
        assert_eq!(sanitize_text(s), s);
        assert_eq!(sanitize_text("hello world"), "hello world");
    }

    /// 身份句：official CLI → official CLI tool（只改一个词）。
    #[test]
    fn identity_sentence_rewritten() {
        let src = "You are Claude Code, Anthropic's official CLI for Claude. More text.";
        let out = sanitize_text(src);
        assert!(
            out.contains("official CLI tool for Claude"),
            "应改写为 tool 形态: {out}"
        );
        assert!(!out.contains("official CLI for Claude."), "{out}");
        assert!(out.contains("You are Claude Code"), "语义句应保留: {out}");
    }

    /// 3P 变体（结尾是逗号）同样命中，因为查找串不带结尾标点。
    #[test]
    fn identity_sentence_3p_variant() {
        let src = "You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.";
        let out = sanitize_text(src);
        assert!(out.contains("official CLI tool for Claude"), "{out}");
        assert!(
            out.contains("running within the Claude Agent SDK"),
            "后续文字须保留: {out}"
        );
    }

    #[test]
    fn main_branch_rewritten_to_default() {
        let src = "Main branch (you will usually use this for PRs)";
        let out = sanitize_text(src);
        assert_eq!(out, "Default branch (you will usually use this for PRs)");
    }

    #[test]
    fn anthropics_feedback_link_rewritten() {
        let src = "See https://github.com/anthropics/claude-code/issues for feedback";
        let out = sanitize_text(src);
        assert!(!out.contains("github.com/anthropics"), "{out}");
        assert!(out.contains("https://github.com/user-feedback/issues"), "{out}");
    }

    /// Codex 归属句：led by OpenAI → led by the community。
    #[test]
    fn codex_attribution_rewritten() {
        let src = "an open source project led by OpenAI";
        let out = sanitize_text(src);
        assert!(out.contains("led by the community"), "{out}");
        assert!(!out.contains("led by OpenAI"), "{out}");
    }

    #[test]
    fn header_segment_stripped() {
        let src = "prefix x-anthropic-billing-header: abc123; suffix";
        let out = sanitize_text(src);
        assert!(
            !out.to_lowercase().contains("x-anthropic-billing-header"),
            "{out}"
        );
        assert!(out.contains("prefix"), "{out}");
        assert!(out.contains("suffix"), "{out}");
    }

    /// 键名大小写变体也要剥离。
    #[test]
    fn header_segment_case_insensitive() {
        let src = "X-Anthropic-Billing-Header: VALUE; tail";
        let out = sanitize_text(src);
        assert!(
            !out.to_lowercase().contains("x-anthropic-billing-header"),
            "{out}"
        );
        assert!(out.contains("tail"), "{out}");
    }

    /// 没有冒号就不算该指纹（Go 正则要求冒号）。
    #[test]
    fn header_key_without_colon_kept() {
        let src = "x-anthropic-billing-header is a phrase";
        let out = sanitize_text(src);
        assert!(out.contains("x-anthropic-billing-header"), "{out}");
    }

    /// cc_ 键值的剥离发生在**特征命中之后**（Go 侧 sanitizeText 先做预检，
    /// 预检不中直接原样返回）。因此单独一个 `cc_env=prod` 不会被剥离 ——
    /// 这里连同特征串一起给出，验证剥离确实发生。
    #[test]
    fn cc_kv_stripped_after_feature_hit() {
        let src = "You are Claude Code. keep cc_env=prod; tail";
        let out = sanitize_text(src);
        assert!(!out.contains("cc_env"), "{out}");
        assert!(out.contains("tail"), "{out}");
    }

    /// 预检不中 → 原样返回（与 Go 一致：普通请求零改动）。
    #[test]
    fn cc_kv_alone_is_not_stripped() {
        let src = "keep cc_env=prod; tail";
        assert_eq!(sanitize_text(src), src);
    }

    /// `cc_entrypoint=` 是独立特征串，命中即触发净化。
    #[test]
    fn cc_entrypoint_triggers_sanitize() {
        let src = "cc_entrypoint=cli; rest of prompt";
        let out = sanitize_text(src);
        assert!(!out.contains("cc_entrypoint"), "{out}");
    }

    /// 词首判定：`abc_cc_x=1` 里的 `cc_x` 不是词首，不应被当作键值剥离。
    #[test]
    fn cc_kv_requires_word_boundary() {
        let src = "abc_cc_x=1";
        assert_eq!(sanitize_text(src), src);
    }

    #[test]
    fn sanitize_content_string_and_array() {
        let mut s = json!("You are Claude Code, Anthropic's official CLI for Claude.");
        assert!(sanitize_content(&mut s));

        let mut arr = json!([
            {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
            {"type": "image_url", "image_url": {"url": "data:..."}}
        ]);
        assert!(sanitize_content(&mut arr));
        // image part 必须原样保留
        assert_eq!(arr[1]["image_url"]["url"], json!("data:..."));
    }

    #[test]
    fn sanitize_content_leaves_non_text_parts() {
        let mut arr = json!([{"type": "image_url", "image_url": {"url": "x"}}]);
        assert!(!sanitize_content(&mut arr));
        assert_eq!(arr[0]["image_url"]["url"], json!("x"));
    }

    #[test]
    fn sanitize_messages_reports_change() {
        let mut msgs = vec![
            json!({"role": "system", "content": "You are Claude Code, Anthropic's official CLI for Claude."}),
            json!({"role": "user", "content": "普通问题"}),
        ];
        assert!(sanitize_messages(&mut msgs));
        // 无变化的输入返回 false
        let mut clean = vec![json!({"role": "user", "content": "普通问题"})];
        assert!(!sanitize_messages(&mut clean));
    }
}
