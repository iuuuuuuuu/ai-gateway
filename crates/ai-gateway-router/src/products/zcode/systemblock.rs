//! 官方 system 块注入 —— Go `internal/zcode/systemblock.go` 的 Rust 移植。
//!
//! # 这个模块解决什么
//!
//! ZCode 的对话通道（start-plan，JWT 账号）要求请求体的**顶层 `system`**
//! 带官方客户端那三块提示词。参考实现（`gakiyukr/zcode2api-plus`，Go，持续更新、
//! 能跑通）在 `upstream/request.go` 里逐字写着：
//!
//! ```text
//! // JWT 账号请求必须注入到顶层 system，否则上游返回 405。
//! ```
//!
//! 而 **405 正是 3012 的载体**（`HTTP 405 {"code":3012,"msg":"request has been
//! blocked due to unusual activity."}`）。所以缺官方 system 时，用户看到的
//! 不是"提示词没生效"这种软症状，而是**整个对话被风控拒掉**。
//!
//! # 为什么现在是默认开启（这段历史必须留着）
//!
//! 本模块原先默认**关闭**，依据是一次 A/B：开/关两组都吃 3012。
//! 但那次 A/B **是在风控期做的** —— 当时该账号连官方客户端都一律 3012。
//! 在"所有请求都失败"的窗口里比较两个方案，结论必然无差别；
//! **那是实验设计的错误，不是方案无效。**
//!
//! 3012 于 2026-09-20 晚自行消失（证实它是有时间尺度的风控、会自己过期），
//! 参考实现无条件注入能跑通 ⇒ 改为默认注入。
//!
//! ⚠ 保留环境变量 `AI_GATEWAY_ZCODE_SYSTEM_BLOCK=0|false` 作为**逃生开关**：
//! 万一注入在某个环境反而出问题，用户能不改代码就关掉。只有**显式**写
//! 0/false 才关，未设置按开启处理。
//!
//! # 形状为什么"恰好 3 块、每块都带 cache_control"
//!
//! Anthropic 只允许 **4 个**缓存断点。官方把 3 个用在 system、第 4 个留给
//! 最后一条消息（后者由官方客户端的 `ApplyClientShape` 负责，本仓未移植）。
//! 多发一块就会超限，而超限的表现是上游报错或静默降级缓存 —— 都不好查。
//!
//! 同理，调用方（Claude Code / DSH / Cline）自己带来的 `cache_control`
//! **必须剥掉**：那是"外来断点"，真实客户端自己拥有整个请求体、从不发外来断点。

use std::sync::OnceLock;

use serde_json::{json, Map, Value};

/// 官方 prompt 资源（从官方 bundle 提取，8487 字节）。
///
/// 用 `include_str!` 编译进二进制而不是运行时读文件：发行包里没有
/// `go-gateway` 目录，运行时读路径必然失败。
const ZCODE_SYSTEM_JSON: &str = include_str!("zcode_system.json");

/// 官方 prompt 资源的反序列化形状。
///
/// ⚠ 字段名与资源里的键**逐字对应**（`camelCase`），不要"改成 Rust 风格"：
/// 改名会让 `serde` 静默取到空串，而空串的表现是"注入了但内容不全"，
/// 比整个失败更难查。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemData {
    pub cli_prefix: String,
    pub stable_sections: Vec<String>,
    pub dynamic_sections: DynamicSections,
    pub environment: EnvironmentLabels,
    pub context_prefix: ContextPrefix,
    pub system_reminder: SystemReminder,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicSections {
    pub before_environment: String,
    pub after_environment: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentLabels {
    pub heading: String,
    pub invoked_line: String,
    pub cwd_label: String,
    pub git_label: String,
    pub git_no: String,
    pub platform_label: String,
    pub shell_label: String,
    pub os_version_label: String,
    pub powered_by_line: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextPrefix {
    pub intro: String,
    pub outro: String,
    pub current_date_heading: String,
    pub current_date_line: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SystemReminder {
    pub open: String,
    pub close: String,
}

/// `environment` 段要用的**实际取值**。
///
/// ⚠ 这些值**必须与请求头一致**（`X-Platform` / `X-Os-Version` 等）。
/// 参考实现明确警告：prompt 里写一套、请求头写另一套，会拼出
/// 「真实客户端不会产生的混合组合」—— 那比不写还糟。
///
/// 注意 Go 侧的调用点是 `BuildAnthropicBodyWithMeta` 里的 `envInfo{}`
/// （**零值**），即只有 `Provider` 由调用链外面填。这不是笔误：请求体翻译层
/// 拿不到本机 cwd / shell / OS 版本，而这些字段取不到时**整行省略**，
/// 所以零值等价于"这条信息没有"。Rust 侧保持同一行为。
#[derive(Debug, Clone, Default)]
pub struct EnvInfo {
    pub cwd: String,
    pub is_git: bool,
    pub platform: String,
    pub shell: String,
    pub os_version: String,
    pub model: String,
    pub provider: String,
}

/// 懒加载官方 prompt 资源。
///
/// 用 `OnceLock` 而不是在启动时解析：解析失败不该让整个网关起不来 ——
/// 这个模块只是一块 prompt，它的失败应当降级成"不注入"，而不是崩溃。
/// 故这里把错误留到真正要用的时候再报。
pub fn load_system_data() -> Result<&'static SystemData, String> {
    static CELL: OnceLock<Result<SystemData, String>> = OnceLock::new();
    let slot = CELL.get_or_init(|| {
        let data: SystemData = serde_json::from_str(ZCODE_SYSTEM_JSON)
            .map_err(|e| format!("解析官方 prompt 资源失败: {e}"))?;
        if data.cli_prefix.trim().is_empty() {
            return Err("官方 prompt 资源里 cliPrefix 为空（资源可能损坏）".to_string());
        }
        Ok(data)
    });
    match slot {
        Ok(d) => Ok(d),
        Err(e) => Err(e.clone()),
    }
}

/// 构造官方形状的 system 块数组，**排在调用方内容前面**。
///
/// 参数 `existing` 是调用方（Claude Code / DSH 等）自己的 system 内容 ——
/// 官方块排在它前面，**不替换**它。这一点很重要：整体替换会丢掉客户端的
/// 项目规范与工具约定。
///
/// 返回的 `Vec<Value>` 可直接塞进 Anthropic 请求体的 `system` 字段。
pub fn build_official_system(
    existing: Option<&Value>,
    model: &str,
    env: &EnvInfo,
) -> Result<Vec<Value>, String> {
    let data = load_system_data()?;

    let stable = data.stable_sections.join("\n\n");
    let dynamic = [
        data.dynamic_sections.before_environment.clone(),
        build_environment_section(data, model, env),
        data.dynamic_sections.after_environment.clone(),
    ]
    .join("\n\n");

    let ephemeral = json!({ "type": "ephemeral" });
    let mut out = vec![
        json!({ "type": "text", "text": data.cli_prefix, "cache_control": ephemeral }),
        json!({ "type": "text", "text": stable, "cache_control": ephemeral }),
        // ⚠ 第 3 块带 "\n\n" 前缀 —— 这是官方形状的一部分，不是笔误。
        // 参考实现逐字保留了它（`text: `\n\n${dynamic}``）。
        json!({ "type": "text", "text": format!("\n\n{dynamic}"), "cache_control": ephemeral }),
    ];

    out.extend(normalize_user_system(existing));
    Ok(out)
}

/// 拼出 `# Environment` 段。
///
/// 形状照官方模板：
///
/// ```text
/// # Environment
/// You have been invoked in the following environment:
///  - Primary working directory: <cwd>
///  - Is a git repository: yes/no
///  - Platform: <win32-x64>
///  - Shell: <bash>
///  - OS Version: <Windows 10 Pro>
///  - You are powered by the model named <provider>/<model>.
/// ```
fn build_environment_section(data: &SystemData, model: &str, env: &EnvInfo) -> String {
    let e = &data.environment;
    let mut b = String::new();
    b.push_str(&e.heading);
    b.push('\n');
    b.push_str(&e.invoked_line);
    b.push('\n');

    push_line(&mut b, &e.cwd_label, &env.cwd);
    // git 是布尔：两个分支都要写（"no" 也是有效信息，省略反而奇怪）
    if env.is_git {
        push_line(&mut b, &e.git_label, "yes");
    } else {
        push_line(&mut b, &e.git_label, &e.git_no);
    }
    push_line(&mut b, &e.platform_label, &env.platform);
    push_line(&mut b, &e.shell_label, &env.shell);
    push_line(&mut b, &e.os_version_label, &env.os_version);

    if !model.trim().is_empty() {
        let powered = e
            .powered_by_line
            .replace("{provider}", &env.provider)
            .replace("{model}", model);
        b.push_str(&powered);
        b.push('\n');
    }
    b.trim_end_matches('\n').to_string()
}

/// 追加一行 ` - <label>: <value>`。
///
/// 取不到就**整行省略**，而不是写 "unknown" —— 假值比缺行更容易被识别成
/// "非真实客户端"（与身份头同一取舍）。
fn push_line(b: &mut String, label: &str, value: &str) {
    if value.trim().is_empty() {
        return;
    }
    b.push_str(" - ");
    b.push_str(label);
    b.push_str(": ");
    b.push_str(value);
    b.push('\n');
}

/// 把调用方自己的 system 统一成块数组。
///
/// 官方客户端只会发块数组；调用方可能发：
///
/// * 字符串   → 包成一个 text 块
/// * 块数组   → 原样保留（但**剥掉它们的 `cache_control`**，见下）
/// * null/其它 → 返回空
///
/// ⚠ 必须剥掉调用方的 `cache_control`：官方 3 块已占用 3 个断点，
/// 加上最后一条消息那个就是 4 个（Anthropic 上限）。客户端自带的
/// 外来断点会让请求**超出上限**，而真实客户端自己拥有整个请求体、
/// 从不发外来断点。
pub fn normalize_user_system(existing: Option<&Value>) -> Vec<Value> {
    let Some(v) = existing else {
        return Vec::new();
    };
    match v {
        Value::Null => Vec::new(),
        Value::String(s) => {
            if s.trim().is_empty() {
                Vec::new()
            } else {
                vec![json!({ "type": "text", "text": s })]
            }
        }
        Value::Array(items) => items.iter().filter_map(strip_cache_control).collect(),
        _ => Vec::new(),
    }
}

/// 克隆一个块并剥掉 `cache_control`（理由见 [`normalize_user_system`]）。
fn strip_cache_control(item: &Value) -> Option<Value> {
    let obj = item.as_object()?;
    let mut clone = Map::new();
    for (k, val) in obj {
        if k == "cache_control" {
            continue; // 剥掉外来断点
        }
        clone.insert(k.clone(), val.clone());
    }
    Some(Value::Object(clone))
}

/// 构造 `meta_user` 首轮消息。
///
/// 官方客户端在第一轮之前插一条 user 消息，内容是 `<system-reminder>` 包住的
/// 当前日期块（**没有内层填充换行**）：
///
/// ```text
/// <system-reminder>
/// As you answer the user's questions, you can use the following context:
/// # currentDate
/// Today's date is 2026-09-19.
///
/// IMPORTANT: this context may or may not be relevant to your tasks. …
/// </system-reminder>
/// ```
///
/// ⚠ **本仓无调用点**：Go 侧也只有 `systemblock_test.go` 调它，生产路径没接。
/// 移植它是为了保持与 Go 的行为一致、便于将来接线，不要在热路径上误用
/// （官方客户端只在**第一轮**插这条消息，每轮都插会改变对话语义）。
pub fn build_context_prefix_message(now: &str) -> Result<Value, String> {
    let data = load_system_data()?;
    let cp = &data.context_prefix;
    let date_line = cp.current_date_line.replace("{date}", now);

    let body = [
        cp.intro.as_str(),
        cp.current_date_heading.as_str(),
        date_line.as_str(),
        "",
        cp.outro.as_str(),
    ]
    .join("\n");

    Ok(json!({
        "role": "user",
        "content": [{
            "type": "text",
            "text": format!("{}\n{}\n{}", data.system_reminder.open, body, data.system_reminder.close),
        }],
    }))
}

/// 报告是否启用官方 system 块注入。
///
/// # ⚠ 现在**恒为 true**（2026-09-20 改），这个函数只保留给测试与诊断
///
/// 原先它读 `AI_GATEWAY_ZCODE_SYSTEM_BLOCK`，默认 **false**（不注入）。
/// 那个默认值的依据是一次 A/B：「开/关两组都 3012」—— **但那次 A/B 是在
/// 风控期做的**，当时该账号连官方客户端都一律吃 3012。在"所有请求都失败"的
/// 窗口里比较两个方案，结论必然无差别；**那是实验设计的错误，不是方案无效。**
///
/// 对照参考实现后确认「JWT 账号请求必须注入到顶层 system，否则上游返回 405」，
/// 而 405 正是 3012 的载体 ⇒ 默认注入。
///
/// ⚠ 保留 `AI_GATEWAY_ZCODE_SYSTEM_BLOCK=0` 作为**逃生开关**：万一注入在某个
/// 环境反而出问题，用户能不改代码就关掉。但那不是默认路径 —— 默认是注入。
pub fn system_block_enabled() -> bool {
    enabled_from_value(std::env::var("AI_GATEWAY_ZCODE_SYSTEM_BLOCK").ok().as_deref())
}

/// [`system_block_enabled`] 的纯函数内核。
///
/// 抽出来是为了**能无副作用地测**：环境变量是进程级的，在并行测试里
/// `set_var` 会与其它测试互相干扰，而"只测默认值"又覆盖不到逃生开关。
fn enabled_from_value(v: Option<&str>) -> bool {
    match v {
        // 未设置 ⇒ 开启（默认路径）
        None => true,
        Some(v) => {
            let v = v.trim();
            // 只有**显式**写 0/false 才关；设成空串等同于未设置。
            if v.is_empty() {
                return true;
            }
            !(v == "0" || v.eq_ignore_ascii_case("false"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_sample() -> EnvInfo {
        EnvInfo {
            cwd: "/tmp/x".to_string(),
            platform: "win32-x64".to_string(),
            shell: "powershell".to_string(),
            os_version: "10.0.26100".to_string(),
            provider: "zhipu".to_string(),
            ..Default::default()
        }
    }

    /// 恰好 3 个块。
    ///
    /// Anthropic 只允许 4 个缓存断点：官方把 3 个用在 system、第 4 个留给
    /// 最后一条消息。**多发一块就会超限**，而超限的表现是上游报错或静默降级
    /// 缓存 —— 都不好查。
    #[test]
    fn official_system_is_exactly_three_blocks() {
        let blocks = build_official_system(None, "GLM-5.3", &env_sample()).expect("构造失败");
        assert_eq!(
            blocks.len(),
            3,
            "官方形状恰好 3 块（Anthropic 缓存断点上限 4，第 4 个留给末条消息），实际 {}",
            blocks.len()
        );
        for (i, b) in blocks.iter().enumerate() {
            assert!(
                b.get("cache_control").is_some(),
                "第 {} 块缺 cache_control（官方每块都带 ephemeral）",
                i + 1
            );
            assert_eq!(b["type"], "text", "第 {} 块 type 应为 text", i + 1);
        }
    }

    /// 第 3 块带 `"\n\n"` 前缀。
    ///
    /// 这是官方形状里最容易被人当"笔误"删掉的一处 —— 参考实现逐字保留了它。
    #[test]
    fn official_system_third_block_has_newline_prefix() {
        let env = EnvInfo {
            provider: "zai".to_string(),
            ..Default::default()
        };
        let blocks = build_official_system(None, "GLM-5.3", &env).expect("构造失败");
        let text = blocks[2]["text"].as_str().unwrap_or_default();
        assert!(
            text.starts_with("\n\n"),
            "第 3 块必须以 \\n\\n 开头（官方形状的一部分，不是笔误），实际开头 {:?}",
            &text[..text.len().min(12)]
        );
    }

    /// 调用方的 system **不被吞掉**。
    ///
    /// 整体替换会丢掉客户端的项目规范与工具约定：官方块排在调用方**前面**，
    /// 不是取而代之。
    #[test]
    fn official_system_keeps_caller_system() {
        let caller = Value::String("PROJECT RULES: 必须用 tabs 缩进".to_string());
        let env = EnvInfo {
            provider: "zai".to_string(),
            ..Default::default()
        };
        let blocks = build_official_system(Some(&caller), "GLM-5.3", &env).expect("构造失败");
        assert_eq!(blocks.len(), 4, "3 个官方块 + 1 个调用方块 = 4");
        assert_eq!(
            blocks[3]["text"], "PROJECT RULES: 必须用 tabs 缩进",
            "调用方的 system 必须原样保留在最后"
        );
    }

    /// 剥掉调用方自带的缓存断点。
    ///
    /// 官方 3 块 + 末条消息 = 4 个断点，正好用满。客户端自带的**外来断点**
    /// 会超出上限 —— 而真实客户端自己拥有整个请求体，从不发外来断点。
    #[test]
    fn official_system_strips_caller_cache_control() {
        let caller = json!([
            { "type": "text", "text": "规则 A", "cache_control": { "type": "ephemeral" } },
            { "type": "text", "text": "规则 B" },
        ]);
        let env = EnvInfo {
            provider: "zai".to_string(),
            ..Default::default()
        };
        let blocks = build_official_system(Some(&caller), "GLM-5.3", &env).expect("构造失败");
        let user = &blocks[3..];
        assert_eq!(user.len(), 2, "调用方 2 块应全部保留");
        for (i, b) in user.iter().enumerate() {
            assert!(
                b.get("cache_control").is_none(),
                "调用方第 {} 块的 cache_control 必须被剥掉（否则超出 4 个断点上限）",
                i + 1
            );
        }
        assert_eq!(user[0]["text"], "规则 A", "剥 cache_control 不该影响 text 内容");
    }

    /// 取不到的值**整行省略**，不写 "unknown"。
    ///
    /// 与身份头同一取舍：填假值比不发更容易被识别成"非真实客户端" ——
    /// 一个真实客户端不会把自己的 OS 版本写成 "unknown"。
    #[test]
    fn environment_section_omits_empty_values() {
        let env = EnvInfo {
            cwd: "/w".to_string(),
            platform: "win32-x64".to_string(),
            // shell / os_version 故意留空
            provider: "zai".to_string(),
            ..Default::default()
        };
        let blocks = build_official_system(None, "GLM-5.3", &env).expect("构造失败");
        let text = blocks[2]["text"].as_str().unwrap_or_default();
        assert!(!text.contains("unknown"), "取不到的值应整行省略，不能写 unknown");
        assert!(text.contains("win32-x64"), "拿到的值应当出现（Platform）");
    }

    /// `meta_user` 的形状。
    #[test]
    fn context_prefix_message_shape() {
        let msg = build_context_prefix_message("2026-09-19").expect("构造失败");
        assert_eq!(msg["role"], "user", "meta_user 的 role 应为 user");
        let content = msg["content"].as_array().expect("content 应为数组");
        assert_eq!(content.len(), 1, "content 应为单块数组");
        let text = content[0]["text"].as_str().unwrap_or_default();
        assert!(
            text.starts_with("<system-reminder>") && text.ends_with("</system-reminder>"),
            "必须以 <system-reminder> 包裹，实际 {:?}",
            &text[..text.len().min(40)]
        );
        assert!(text.contains("2026-09-19"), "日期应被替换进文案");
    }

    /// 默认开启（2026-09-20 反转的契约）。
    ///
    /// 原先守的是「不要把已证伪的假设默认打开」，但那次 A/B 是在风控期做的。
    /// 详见 [`system_block_enabled`] 的文档。
    #[test]
    fn system_block_enabled_by_default() {
        assert!(enabled_from_value(None), "默认必须**开启**");
        assert!(enabled_from_value(Some("")), "空串等同未设置 ⇒ 开启");
        for v in ["1", "true", "TRUE", " true "] {
            assert!(enabled_from_value(Some(v)), "{v:?} 应视为开启");
        }
        for v in ["0", "false", "FALSE", " false "] {
            assert!(!enabled_from_value(Some(v)), "{v:?} 应视为关闭（逃生开关）");
        }
    }

    /// 资源解析出来的关键锚点值（防止有人"顺手"改 JSON 或改字段名）。
    #[test]
    fn system_resource_parses_with_expected_anchors() {
        let d = load_system_data().expect("官方 prompt 资源应能解析");
        assert_eq!(d.cli_prefix, "You are ZCode, an interactive coding agent");
        assert_eq!(d.stable_sections.len(), 2, "stableSections 是 2 段");
        assert_eq!(d.environment.heading, "# Environment");
        assert_eq!(d.environment.git_no, "no");
        assert_eq!(d.context_prefix.current_date_heading, "# currentDate");
        assert_eq!(d.system_reminder.open, "<system-reminder>");
        assert_eq!(d.system_reminder.close, "</system-reminder>");
        assert!(
            d.environment.powered_by_line.contains("{provider}")
                && d.environment.powered_by_line.contains("{model}"),
            "poweredByLine 的占位符不能被改掉"
        );
    }

    /// `provider`/`model` 替换进 poweredByLine；model 为空则整行不发。
    #[test]
    fn powered_by_line_uses_provider_and_model() {
        let env = EnvInfo {
            provider: "zai".to_string(),
            ..Default::default()
        };
        let blocks = build_official_system(None, "glm-5.3", &env).expect("构造失败");
        let text = blocks[2]["text"].as_str().unwrap_or_default();
        assert!(text.contains("zai/glm-5.3"), "应替换成 provider/model，实际: {text}");

        let blocks2 = build_official_system(None, "", &env).expect("构造失败");
        let text2 = blocks2[2]["text"].as_str().unwrap_or_default();
        assert!(!text2.contains("powered by"), "model 为空时整行不发");
    }
}
