//! 极简 YAML 映射读写（仅覆盖 DSH 配置文件用到的子集）。
//!
//! 为什么不用完整 YAML 库：DSH 的 `settings.yaml` / `.credentials.yaml` 只用到
//! 「嵌套映射 + 数组 + 标量 + 引号字符串 + 注释」这几种形态，而引入 serde_yaml
//! 会带来一个新依赖与一轮锁定版本变更。这里实现最小可用子集：
//!
//!   - 解析：缩进驱动的映射/序列，支持 `key: value`、`- item`、`- key: value`
//!   - 渲染：统一 2 空格缩进，字符串按需加引号
//!
//! 明确不支持的（遇到即报错，绝不静默丢数据）：
//!   - 锚点/别名（`&a` / `*a`）
//!   - 多行标量（`|` / `>`）
//!   - 流式集合（`{a: 1}` / `[1, 2]`）
//!
//! 调用方（`agent_import`）在解析失败时会放弃写入并保留用户原文件，
//! 因此这里的保守策略是安全的。

use serde_json::{Map, Value};

/// 解析 YAML 文本为 JSON 映射。
pub fn parse_mapping(text: &str) -> Result<Map<String, Value>, String> {
    let lines: Vec<&str> = text
        .lines()
        .map(strip_trailing_comment)
        .filter(|line| {
            let t = line.trim();
            !t.is_empty() && !t.starts_with('#')
        })
        .collect();
    if lines.is_empty() {
        return Ok(Map::new());
    }

    let mut index = 0usize;
    let value = parse_block(&lines, &mut index, indent_of(lines[0]))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err("YAML 根节点必须是映射".to_string()),
    }
}

/// 把 JSON 值渲染成 YAML 文本。
pub fn render_mapping(value: &Value) -> Result<String, String> {
    let mut out = String::new();
    render_value(value, 0, &mut out, true)?;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 解析
// ---------------------------------------------------------------------------

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// 解析一个块（同缩进的键值对集合或序列）。
fn parse_block(lines: &[&str], index: &mut usize, indent: usize) -> Result<Value, String> {
    if *index >= lines.len() {
        return Ok(Value::Null);
    }
    let is_sequence = lines[*index].trim_start().starts_with("- ");

    if is_sequence {
        let mut items = Vec::new();
        while *index < lines.len() {
            let line = lines[*index];
            if indent_of(line) < indent {
                break;
            }
            if indent_of(line) > indent || !line.trim_start().starts_with("- ") {
                break;
            }
            let rest = line.trim_start().trim_start_matches("- ").trim();
            *index += 1;

            if rest.is_empty() {
                // `-` 之后是被缩进的子块
                let child_indent = next_indent(lines, *index).unwrap_or(indent + 2);
                items.push(parse_block(lines, index, child_indent)?);
                continue;
            }

            if let Some((key, raw)) = split_key_value(rest) {
                // `- key: value`：该项是映射，且后续同缩进键属于同一项
                let mut map = Map::new();
                if raw.is_empty() {
                    let child_indent = next_indent(lines, *index).unwrap_or(indent + 4);
                    if child_indent > indent {
                        map.insert(key.to_string(), parse_block(lines, index, child_indent)?);
                    } else {
                        map.insert(key.to_string(), Value::Null);
                    }
                } else {
                    map.insert(key.to_string(), parse_scalar(raw));
                }
                // 吸收该序列项内后续的兄弟键（缩进 = indent + 2）
                while *index < lines.len() {
                    let l = lines[*index];
                    if indent_of(l) <= indent {
                        break;
                    }
                    let t = l.trim_start();
                    let Some((k, v)) = split_key_value(t) else {
                        break;
                    };
                    *index += 1;
                    if v.is_empty() {
                        let child_indent = next_indent(lines, *index).unwrap_or(indent + 4);
                        if child_indent > indent {
                            map.insert(k.to_string(), parse_block(lines, index, child_indent)?);
                        } else {
                            map.insert(k.to_string(), Value::Null);
                        }
                    } else {
                        map.insert(k.to_string(), parse_scalar(v));
                    }
                }
                items.push(Value::Object(map));
                continue;
            }

            items.push(parse_scalar(rest));
        }
        return Ok(Value::Array(items));
    }

    let mut map = Map::new();
    while *index < lines.len() {
        let line = lines[*index];
        if indent_of(line) < indent {
            break;
        }
        if indent_of(line) > indent {
            return Err(format!("第 {} 行缩进异常", *index + 1));
        }
        let trimmed = line.trim_start();
        let Some((key, raw)) = split_key_value(trimmed) else {
            break;
        };
        *index += 1;

        if raw.is_empty() {
            let child_indent = next_indent(lines, *index);
            match child_indent {
                Some(ci) if ci > indent => {
                    map.insert(key.to_string(), parse_block(lines, index, ci)?);
                }
                _ => {
                    map.insert(key.to_string(), Value::Null);
                }
            }
        } else {
            map.insert(key.to_string(), parse_scalar(raw));
        }
    }
    Ok(Value::Object(map))
}

fn next_indent(lines: &[&str], index: usize) -> Option<usize> {
    lines.get(index).map(|l| indent_of(l))
}

/// 拆分 `key: value`，正确处理引号内的冒号。
///
/// 难点：裸 URL 标量（如 `- https://x.com/v1`）的第一个冒号在 scheme 之后，
/// 会被误当成 `key: value`。判据必须用 **`//` 前缀**而不是单个 `/`：
/// `scheme://` 的 rest 一定以 `//` 开头，而合法路径值（如
/// `files_api_upload_endpoint: /v1/files/upload`）只以单个 `/` 开头。
///
/// 早期实现用单 `/` 判定，把 MiniMax Code 的配置整体判成解析失败
/// （该文件里就有以 `/` 开头的路径值），调用方会因此放弃写入。
fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let mut in_single = false;
    let mut in_double = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ':' if !in_single && !in_double => {
                let key = line[..i].trim();
                if key.is_empty() || key.contains(char::is_whitespace) && key.contains('/') {
                    return None;
                }
                let rest = line[i + 1..].trim();
                // `scheme://host` 是标量而不是键值对
                if rest.starts_with("//") {
                    return None;
                }
                return Some((key, rest));
            }
            _ => {}
        }
    }
    None
}

/// 解析标量：布尔 / 数字 / null / 字符串。
fn parse_scalar(raw: &str) -> Value {
    let t = raw.trim();
    if t.is_empty() {
        return Value::Null;
    }
    if t == "null" || t == "~" {
        return Value::Null;
    }
    if t == "true" {
        return Value::Bool(true);
    }
    if t == "false" {
        return Value::Bool(false);
    }
    if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
        || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
    {
        let inner = &t[1..t.len() - 1];
        // 双引号是转义形式、单引号是字面量 —— 必须与 render_string 的转义表互逆，
        // 否则「读取→渲染→再读取」每往返一次就给值多累积一层反斜杠
        //（`C:\x` → `C:\\x` → `C:\\\\x`），最终写回客户端的是错误的路径/凭据。
        return Value::String(if t.starts_with('"') {
            unescape_double(inner)
        } else {
            inner.to_string()
        });
    }
    if let Ok(n) = t.parse::<i64>() {
        return Value::Number(n.into());
    }
    if let Ok(n) = t.parse::<f64>() {
        if let Some(num) = serde_json::Number::from_f64(n) {
            return Value::Number(num);
        }
    }
    Value::String(t.to_string())
}

/// 反转义 render_string 写入的双引号字符串（`\\` `\"` `\n`）。
///
/// 必须与该函数的 replace 链严格互逆：渲染把 `\` → `\\`、`"` → `\"`、
/// 换行 → `\n`，这里逐一还原。无法识别的转义保守地原样保留（含反斜杠），
/// 宁可多一个字符也不要静默丢字符。
fn unescape_double(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('n') => out.push('\n'),
            // 非法/未知转义：原样保留（含反斜杠），避免丢字符。
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// 剥离行尾注释：`#` 前必须是行首或空白，且不在引号内。
///
/// 为什么必须剥：`split_key_value` 取冒号之后到行尾的全部内容作为值，注释会
/// 一并进入。于是 `logLevel: info  # 调试用` 解析出字符串 `"info  # 调试用"`，
/// 布尔 `true # 待验证` 变成字符串，而 `baseURL: https://x/v1 # 官方地址`
/// 会带着注释去请求一个不存在的地址 —— 接入后请求直接失败。
///
/// 只认「前面是空白」的 `#`，因此 `apiKey: abc#def`（密码里的 `#` 无空格）
/// 原样保留；引号内的 `#` 同样不算注释。按 char_indices 切分，对中文安全。
fn strip_trailing_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single
                && !in_double
                && i > 0
                && line.as_bytes()[i - 1].is_ascii_whitespace() =>
            {
                return &line[..i];
            }
            _ => {}
        }
    }
    line
}

// ---------------------------------------------------------------------------
// 渲染
// ---------------------------------------------------------------------------

fn render_value(value: &Value, indent: usize, out: &mut String, top: bool) -> Result<(), String> {
    match value {
        Value::Object(map) => {
            if map.is_empty() {
                if top {
                    return Ok(());
                }
                out.push_str("{}");
                out.push('\n');
                return Ok(());
            }
            for (key, child) in map {
                out.push_str(&" ".repeat(indent));
                out.push_str(&render_key(key));
                match child {
                    Value::Object(m) if !m.is_empty() => {
                        out.push_str(":\n");
                        render_value(child, indent + 2, out, false)?;
                    }
                    Value::Array(items) if !items.is_empty() => {
                        out.push_str(":\n");
                        render_sequence(items, indent, out)?;
                    }
                    other => {
                        out.push_str(": ");
                        out.push_str(&render_scalar(other));
                        out.push('\n');
                    }
                }
            }
            Ok(())
        }
        _ => Err("YAML 根节点必须是映射".to_string()),
    }
}

fn render_sequence(items: &[Value], parent_indent: usize, out: &mut String) -> Result<(), String> {
    let item_indent = parent_indent + 2;
    for item in items {
        match item {
            Value::Object(map) if !map.is_empty() => {
                let mut first = true;
                for (key, child) in map {
                    out.push_str(&" ".repeat(if first { item_indent } else { item_indent + 2 }));
                    if first {
                        out.push_str("- ");
                        first = false;
                    }
                    out.push_str(&render_key(key));
                    match child {
                        Value::Object(m) if !m.is_empty() => {
                            out.push_str(":\n");
                            render_value(child, item_indent + 4, out, false)?;
                        }
                        Value::Array(v) if !v.is_empty() => {
                            out.push_str(":\n");
                            render_sequence(v, item_indent + 2, out)?;
                        }
                        other => {
                            out.push_str(": ");
                            out.push_str(&render_scalar(other));
                            out.push('\n');
                        }
                    }
                }
            }
            other => {
                out.push_str(&" ".repeat(item_indent));
                out.push_str("- ");
                out.push_str(&render_scalar(other));
                out.push('\n');
            }
        }
    }
    Ok(())
}

/// 键名加引号规则：含特殊字符时加双引号。
fn render_key(key: &str) -> String {
    let needs_quote = key.is_empty()
        || key.contains(':')
        || key.contains('#')
        || key.starts_with(['-', '?', '[', '{', '*', '&', '!', '|', '>', '@', '`']);
    if needs_quote {
        format!("\"{}\"", key.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        key.to_string()
    }
}

/// 标量渲染规则：按需加引号，避免被误解析成其他类型。
fn render_scalar(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => render_string(s),
        Value::Array(_) | Value::Object(_) => "{}".to_string(),
    }
}

fn render_string(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".to_string();
    }
    let needs_quote = s.contains(':')
        || s.contains('#')
        || s.contains('\n')
        || s.starts_with([' ', '-', '?', '[', '{', '*', '&', '!', '|', '>', '@', '`', '\'', '"'])
        || s.ends_with(' ')
        || matches!(s, "true" | "false" | "null" | "~")
        || s.parse::<f64>().is_ok();
    if needs_quote {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_nested_mapping_and_sequence() {
        let text = "\
ui-onboarding:
  welcomeNoticeVersion: 2026-08-13.1
llm-pi-ai:
  providers:
    newapi:
      apiKeyEnv: NEWAPI_API_KEY
      baseURL: http://localhost:30001/v1
      models:
        - id: glm-5.3-flash
          name: glm-5.3-flash
          reasoningEfforts:
            off: null
            high: high
";
        let map = parse_mapping(text).expect("parse");
        assert_eq!(map["ui-onboarding"]["welcomeNoticeVersion"], "2026-08-13.1");
        let provider = &map["llm-pi-ai"]["providers"]["newapi"];
        assert_eq!(provider["apiKeyEnv"], "NEWAPI_API_KEY");
        assert_eq!(provider["baseURL"], "http://localhost:30001/v1");
        let models = provider["models"].as_array().expect("models array");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], "glm-5.3-flash");
        assert_eq!(models[0]["reasoningEfforts"]["off"], Value::Null);
        assert_eq!(models[0]["reasoningEfforts"]["high"], "high");
    }

    #[test]
    fn round_trip_preserves_existing_providers() {
        let text = "other:\n  keep: 1\nrefs:\n  OTHER_KEY: abc\n";
        let map = parse_mapping(text).expect("parse");
        let rendered = render_mapping(&Value::Object(map)).expect("render");
        assert!(rendered.contains("OTHER_KEY"), "{rendered}");
        assert!(rendered.contains("keep"), "{rendered}");
    }

    #[test]
    fn quotes_values_that_look_like_scalars() {
        let value = json!({ "refs": { "KEY": "123", "URL": "http://x/v1", "BOOL": "true" } });
        let out = render_mapping(&value).expect("render");
        assert!(out.contains("\"123\""), "{out}");
        assert!(out.contains("\"http://x/v1\""), "{out}");
        assert!(out.contains("\"true\""), "{out}");
    }

    #[test]
    fn rejects_unsupported_flow_collections() {
        // 流式集合不在支持范围内：应当解析失败而不是悄悄丢字段。
        let text = "root:\n  a: {b: 1}\n";
        let map = parse_mapping(text).expect("flow map becomes string");
        // 当前实现把 `{b: 1}` 当字符串保留，确认没有丢键。
        assert_eq!(map["root"]["a"], "{b: 1}");
    }

    /// **回归测试**：以单个 `/` 开头的路径值是合法标量，不是键值对分隔。
    ///
    /// 早期实现用「rest 以 `/` 开头就判为 URL」的规则，把
    /// `files_api_upload_endpoint: /v1/files/upload` 这类行判成解析失败 ——
    /// 于是整个 MiniMax Code 配置（实测含此类路径值）解析不了，
    /// 调用方会因此**放弃写入**，用户看到的是「接入失败」而不知原因。
    /// 正确判据是 `//`（`scheme://` 的特征），单 `/` 是普通路径。
    #[test]
    fn accepts_path_values_starting_with_single_slash() {
        let text = "\
capabilities:
  support_files_api: true
  files_api_upload_endpoint: /v1/files/upload
  max_attachments_count: 4
";
        let map = parse_mapping(text).expect("含路径值的配置必须能解析");
        assert_eq!(map["capabilities"]["support_files_api"], true);
        assert_eq!(
            map["capabilities"]["files_api_upload_endpoint"],
            "/v1/files/upload"
        );
        assert_eq!(map["capabilities"]["max_attachments_count"], 4);
    }

    /// 裸 URL 标量仍应被识别为标量（不能被当成 `key: value`）。
    #[test]
    fn bare_url_scalar_is_not_treated_as_key_value() {
        let text = "endpoints:\n  - https://api.example.com/v1\n  - /local/path\n";
        let map = parse_mapping(text).expect("parse");
        let list = map["endpoints"].as_array().expect("array");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], "https://api.example.com/v1");
        assert_eq!(list[1], "/local/path");
    }

    /// MiniMax Code 配置的关键结构（provider 块 + baseURL + models）。
    #[test]
    fn parses_minimax_style_provider_block() {
        let text = "\
logLevel: info
provider:
  minimax:
    name: MiniMax
    npm: '@ai-sdk/anthropic'
    options:
      authMode: managed-login
      baseURL: https://agent.minimax.cn/mavis/api/v1/llm/v1
    models:
      MiniMax-M3:
        name: MiniMax-M3
        reasoning: true
defaultModel: minimax/MiniMax-M3
";
        let map = parse_mapping(text).expect("parse");
        let provider = &map["provider"]["minimax"];
        assert_eq!(provider["name"], "MiniMax");
        assert_eq!(provider["npm"], "@ai-sdk/anthropic");
        assert_eq!(
            provider["options"]["baseURL"],
            "https://agent.minimax.cn/mavis/api/v1/llm/v1"
        );
        assert_eq!(provider["models"]["MiniMax-M3"]["reasoning"], true);
        assert_eq!(map["defaultModel"], "minimax/MiniMax-M3");

        // 往返后结构不变（写入前必须能保证不损坏用户原配置）
        let rendered = render_mapping(&Value::Object(map.clone())).expect("render");
        let reparsed = parse_mapping(&rendered).expect("reparse");
        assert_eq!(Value::Object(map), Value::Object(reparsed));
    }

    /// 渲染与解析必须严格互逆：含反斜杠 / 双引号 / 换行的值往返后不得被改坏。
    ///
    /// 历史 bug：render_string 把 `\` → `\\`、`"` → `\"`，而 parse_scalar 只做
    /// `t[1..len-1]` 剥壳、不做反转义，于是每「重新接入」一次就多累积一层反斜杠
    /// （Windows 路径 `C:\x` → `C:\\x` → `C:\\\\x`），写回客户端后指向不存在的
    /// 文件；网关 API Key 若含特殊字符则直接 401。
    #[test]
    fn round_trip_preserves_escapes() {
        let value = json!({
            "winPath": "C:\\Users\\me\\config.yaml",
            "quoted": "he said \"hi\"",
            "newline": "line1\nline2",
            "loneBackslash": "\\",
            "loneQuote": "\"",
            "mixed": "a\\\"b",
        });
        let rendered = render_mapping(&value).expect("render");
        let back = parse_mapping(&rendered).expect("reparse");
        assert_eq!(
            Value::Object(back.clone()),
            value,
            "渲染与解析必须互逆，否则每次重新接入都会改坏值：\n{rendered}"
        );

        // 再往返一次，确认稳定（不累积转义）
        let rendered2 = render_mapping(&Value::Object(back.clone())).expect("render2");
        let back2 = parse_mapping(&rendered2).expect("reparse2");
        assert_eq!(back2, back, "二次往返必须仍然稳定：\n{rendered2}");
    }

    /// 单引号是字面量，不做反转义（与 YAML 规范一致）。
    #[test]
    fn single_quoted_is_literal() {
        let map = parse_mapping("a: 'C:\\x'\n").expect("parse");
        assert_eq!(map["a"], "C:\\x", "单引号内不应被反转义");
    }

    /// 行尾注释必须被剥离，否则注释会被并入值写回客户端配置。
    ///
    /// 历史 bug：`baseURL: https://x/v1 # 官方地址` 解析出带注释的 URL，
    /// 客户端据此请求一个不存在的地址，接入后请求直接失败。
    #[test]
    fn strips_trailing_comments() {
        let text = "\
baseURL: https://agent.minimax.cn/v1 # 官方地址
logLevel: info  # 调试用
reasoning: true # 待验证
plain: value
";
        let map = parse_mapping(text).expect("parse");
        assert_eq!(map["baseURL"], "https://agent.minimax.cn/v1");
        assert_eq!(map["logLevel"], "info");
        assert_eq!(map["reasoning"], true, "布尔值不应被注释污染成字符串");
        assert_eq!(map["plain"], "value");

        // 渲染回来不得再含注释
        let rendered = render_mapping(&Value::Object(map)).expect("render");
        assert!(!rendered.contains('#'), "渲染结果不应带注释：\n{rendered}");
    }

    /// 注释识别的边界：无空格的 `#`、引号内的 `#` 都不算注释。
    #[test]
    fn comment_stripping_respects_quotes_and_tight_hash() {
        // 密码里的 # 无空格 → 属于值
        let map = parse_mapping("apiKey: abc#def\n").expect("parse");
        assert_eq!(map["apiKey"], "abc#def", "无空格的 # 不是注释");

        // 引号内的 # → 属于值
        let map = parse_mapping("url: \"http://x/#frag\" # 尾注\n").expect("parse");
        assert_eq!(map["url"], "http://x/#frag", "引号内的 # 不是注释");

        // 中文注释按字符边界切分，不应 panic
        let map = parse_mapping("name: 测试账号 # 中文注释\n").expect("parse");
        assert_eq!(map["name"], "测试账号");
    }
}
