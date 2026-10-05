//! 网关自有系统提示词：内置默认 + 文件覆盖，并负责出站改写。
//!
//! # 背景（为什么需要这个模块）
//!
//! 客户端（Claude Code / Codex 等 CLI）会在 system 提示里注入固定模板句，上游
//! 内容审核按**逐字精确匹配**拦截，合法流量被误杀。做法是网关在出站前用自有
//! 系统提示词**整体替换**（custom）或**追加**（append）客户端的 system/developer
//! 消息，从源头消灭 system 来源的指纹误报。
//!
//! # 与 [`crate::upstream`] 的 `sanitize` 的关系
//!
//! 两层叠加、互不替代 —— sanitize 负责清洗 user/assistant 消息里的指纹串，本模块
//! 负责 system/developer 消息的整体替换。用户明确配了提示词文件时，sanitize 仍
//! 照常工作。
//!
//! # 移植自
//!
//! `go-gateway/internal/prompt/prompt.go`（252 行）+ `defaultprompt.md`（843 B）
//! + `prompt_test.go`（395 行）+ `append_test.go`（215 行）。调用点在
//! `go-gateway/internal/server/forward.go:250-263`，Rust 对应
//! [`crate::server`] 的 `run_forward`。

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Map;
use std::collections::BTreeMap;

use crate::error::{GatewayError, Result};

/// 内置默认提示词（custom/append 模式下未指定 `prompt.file` 时使用）。
///
/// ★ 用 `include_str!` 而不是写成 Rust 字符串常量：提示词是**文本资产**，独立成
/// `.md` 文件才能让非 Rust 作者（如使用者本人）直接读改、diff 与审阅，也避免在
/// Rust 源码里堆多行字符串字面量。与 Go 侧 `//go:embed defaultprompt.md` 同口径，
/// 也与本仓库既有先例
/// （`products/zcode/systemblock.rs:48 include_str!("zcode_system.json")`）一致。
///
/// ★ 该文件与 `go-gateway/internal/prompt/defaultprompt.md` **必须逐字一致**
/// （SHA256 `F2383BD3D1CD88D4F9D4322627F280211CB772E2B921F6C9BF293028CB310C1B`）：
/// 两个网关是 A/B 对照关系，默认提示词若分叉，「同一请求两个网关给出不同答案」
/// 就多出一个无法解释的变量。
const DEFAULT_PROMPT: &str = include_str!("defaultprompt.md");

/// 降级提示词：刻意极简中性，供「误报重试」场景使用。
///
/// 触发场景：请求被上游内容策略拦截、且判定为 system 来源的**指纹误报**时，换这段
/// 最小提示词重试一次，绕开误报而不改变用户指令的合法性语义。
///
/// ⚠ 与 Go 侧同样**尚未接入任何自动重试分支**：本仓库的 `upstream.Classify`
/// 目前**没有**内容拦截这一分类（无实测样本可依据），因此该常量只是「能力就绪」。
/// 接入需要先拿到可复现的拦截样本，否则就是在猜上游行为。
///
/// 保留它（尽管当前是 dead 的）是为了与 Go 侧保持逐字对等：将来任一侧接入时，
/// 另一侧不该因为「常量不存在」而被迫重新决定文案。
pub const DEGRADED: &str = "You are a helpful assistant. Respond in the user's language, follow the user's instructions, and be direct and concise.";

/// 透传客户端原始 system（**缺省**，既有行为不变）。
pub const MODE_PASSTHROUGH: &str = "passthrough";

/// 用网关自有提示词替换客户端的 system/developer 消息。
pub const MODE_CUSTOM: &str = "custom";

/// 在客户端**已有**的 system 消息之后**追加**网关提示词。
///
/// 与 `custom` 的区别：`custom` 会丢掉客户端的项目规范（模型于是不遵守那些约定，
/// 表现为「变笨了」）；`append` 让两者共存。见 [`append`] 的说明。
pub const MODE_APPEND: &str = "append";

/// 返回内置默认提示词文本。
///
/// 单独暴露（而不是让调用方用 `load("")`）是为了让「内置默认」可被显式读取：
/// 测试与运维都想确认当前内置文本到底是哪一版。
pub fn default_prompt() -> &'static str {
    DEFAULT_PROMPT
}

/// 按 `file` 加载系统提示词文本。
///
/// - `file` 非空 → 读该文件；不存在 / 读失败 / 内容为空 → 返回 error，调用方 fail fast；
/// - `file` 空 → 返回内置默认 [`DEFAULT_PROMPT`]。
///
/// # 为什么不把 mode 也传进来
///
/// mode 决定「用不用」这段文本，属于路由语义，由调用方
/// （[`crate::config::Config::normalize`] / `run_forward`）判断；`load` 只负责
/// 「拿到一段提示词文本」。多传一个恒不被使用的参数，只会让签名谎称它关心 mode。
///
/// # 为什么读失败要 fail fast（而不是静默回落内置默认）
///
/// 用户**明确**配了文件，说明他要的就是那份提示词；静默换成别的文本会表现为
/// 「配了却像没配」—— 现象与原因隔了十万八千里，极难排查。启动即报错能立刻暴露
/// 路径写错/文件缺失。
///
/// # 为什么内容为空也算错误
///
/// 空提示词会让 [`rewrite`] 变成空操作（systemPrompt 为空时它原样返回），最终等价
/// 于 passthrough —— 用户以为在用自有提示词，实际客户端的 system 一条没动。这同样
/// 是「静默用错」，故与读失败同等对待。
pub fn load(file: &str) -> Result<String> {
    if file.trim().is_empty() {
        return Ok(DEFAULT_PROMPT.to_string());
    }
    let raw = std::fs::read(file)
        .map_err(|e| GatewayError::ConfigInvalid(format!("prompt.file {file}: {e}")))?;
    // Go 是 `string(raw)`：非法 UTF-8 会被 Go 静默替换成 U+FFFD 并继续跑。
    // Rust 侧选择**报错**而不是 lossy 替换 —— 提示词是要逐字发给上游的文本，
    // 悄悄改动它的字节比拒绝启动更糟（且与「读失败 fail fast」同一口径）。
    let text = String::from_utf8(raw).map_err(|e| {
        GatewayError::ConfigInvalid(format!("prompt.file {file}: 文件不是合法 UTF-8：{e}"))
    })?;
    if text.trim().is_empty() {
        return Err(GatewayError::ConfigInvalid(format!(
            "prompt.file {file}: 文件内容为空（空提示词等价于不改写，\
             请写入提示词文本，或把 prompt.mode 改回 passthrough）"
        )));
    }
    // 按**原文**返回（不 trim）：提示词里可能有意保留的缩进/换行结构，
    // 只把「是否为空」交给 trim 判断。
    Ok(text)
}

/// 把请求体的系统提示词整体替换为 `system_prompt`：
///
/// - 删除 `messages` 中所有 role 为 `system` / `developer` 的消息；
/// - 在 `messages` **头部插入**一条 `{"role":"system","content":system_prompt}`；
/// - `messages` 之外的所有字段逐字不动，保留下来的 user/assistant/tool 消息也逐字不动。
///
/// 解析失败 → 原样返回（**绝不失败**）：`rewrite` 位于转发关键路径上，任何解析错误
/// 都不该阻塞请求，让上游按其原始语义处理即可 —— 宁可少做一次改写，也不能把请求体
/// 改坏或让请求失败。
pub fn rewrite(body: &[u8], system_prompt: &str) -> Vec<u8> {
    // 空提示词 = 不改写：插入一条空 system 只会污染请求，语义上也无意义。
    if body.is_empty() || system_prompt.trim().is_empty() {
        return body.to_vec();
    }
    let Some(mut doc) = parse_doc(body) else {
        return body.to_vec();
    };
    let Some(sys_msg) = system_message(system_prompt) else {
        return body.to_vec();
    };

    let msgs = parse_messages(&doc);
    // 头部插入：自有 system 永远在第一位。
    let mut kept: Vec<String> = Vec::with_capacity(msgs.len() + 1);
    kept.push(sys_msg);
    for m in &msgs {
        if is_system_like(m) {
            continue; // 旧 system/developer 整体丢弃
        }
        kept.push(m.get().to_string());
    }

    let Some(encoded) = encode_array(&kept) else {
        return body.to_vec();
    };
    let Ok(arr) = RawValue::from_string(encoded) else {
        return body.to_vec();
    };
    doc.insert("messages".to_string(), arr);
    encode_object(&doc).unwrap_or_else(|| body.to_vec())
}

/// 在客户端**已有**的 system 消息**之后**追加网关提示词。
///
/// # 为什么需要第三种模式（`custom` 的已知副作用）
///
/// [`rewrite`]（custom 模式）把客户端的 system **整体替换**掉。代价是：客户端的
/// **项目规范**（代码风格、工具约定、「不要改 X 文件」这类约束）一并消失，模型开始
/// 不遵守那些约定 —— 用户看到的是「模型变笨了 / 不听话了」，而原因在网关把它的
/// 规则删了。
///
/// `append` 让两者共存：客户端规则在前、网关提示词在后。
///
/// # 为什么插在**开头连续的** system 块之后，而不是末尾
///
/// 放在末尾会破坏客户端的**前缀缓存**（Anthropic / OpenAI 都按前缀命中）：把网关
/// 提示词插在中间，后面所有内容的前缀都变了。插在开头那组 system 之后，客户端原有
/// 的稳定前缀**逐字保留**。
///
/// ⚠ 只跳过**开头连续**的 system/developer：中间的 system 消息（有些 agentic
/// 客户端会在对话中途注入）不参与，否则会把网关提示词插到对话中间，语义上等于
/// 「用户说到一半时改了系统规则」。
pub fn append(body: &[u8], system_prompt: &str) -> Vec<u8> {
    if body.is_empty() || system_prompt.trim().is_empty() {
        return body.to_vec();
    }
    let Some(mut doc) = parse_doc(body) else {
        return body.to_vec();
    };
    let Some(sys_msg) = system_message(system_prompt) else {
        return body.to_vec();
    };

    let msgs = parse_messages(&doc);
    let mut kept: Vec<String> = Vec::with_capacity(msgs.len() + 1);
    let mut inserted = false;
    for m in &msgs {
        // 在**开头连续的** system 块之后插入（见上面的理由）。
        if !inserted && !is_system_like(m) {
            kept.push(sys_msg.clone());
            inserted = true;
        }
        kept.push(m.get().to_string());
    }
    // 全是 system 或没有 messages：追加到末尾。
    //
    // ⚠ 这一条**不能省**：漏了会让网关提示词**静默不插入**（配置看起来生效了、
    // 实际什么都没追加）。
    if !inserted {
        kept.push(sys_msg);
    }

    let Some(encoded) = encode_array(&kept) else {
        return body.to_vec();
    };
    let Ok(arr) = RawValue::from_string(encoded) else {
        return body.to_vec();
    };
    doc.insert("messages".to_string(), arr);
    encode_object(&doc).unwrap_or_else(|| body.to_vec())
}

/// 判断一条消息的 role 是否为 `system` / `developer`。
///
/// 比较用「去空白 + 大小写不敏感」，与 `upstream.normalizeRoles` 处理 developer 的
/// 口径保持一致：客户端拼写变体（`"System"`、`"system "`）不该让旧提示词漏删 ——
/// 漏删意味着客户端指纹留在请求里，正好绕过本功能的目的。
///
/// 非法 JSON 或非对象的消息一律**不**当 system（保守保留）：宁可多留一条，也不要
/// 误删用户的实际内容。
fn is_system_like(raw: &RawValue) -> bool {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(default)]
        role: String,
    }
    let Ok(probe) = serde_json::from_str::<Probe>(raw.get()) else {
        return false;
    };
    matches!(
        probe.role.trim().to_lowercase().as_str(),
        "system" | "developer"
    )
}

/// 把提示词序列化成一条 system 消息的 JSON 文本。
///
/// 交给序列化器而不是手工拼 JSON：提示词里可能有引号、换行、反斜杠，手工拼会拼出
/// 非法转义。
fn system_message(system_prompt: &str) -> Option<String> {
    let mut m: Map<String, serde_json::Value> = Map::new();
    m.insert("role".to_string(), serde_json::Value::from("system"));
    m.insert(
        "content".to_string(),
        serde_json::Value::from(system_prompt),
    );
    serde_json::to_string(&m).ok()
}

/// 顶层对象：`键 → 原始 JSON 文本`。
///
/// ★ 用 `BTreeMap` 而不是 `serde_json::Map`：后者只为 `Value` 实现了
/// `Deserialize`/`Serialize`/`insert`/`get`（它内部就是 `Map<String, Value>`），
/// 装不下 `Box<RawValue>`。`BTreeMap` 的泛型实现可用，且**同样是按键字典序**，
/// 与下面 [`encode_object`] 的排序假设一致。
type Doc = BTreeMap<String, Box<RawValue>>;

/// 解析顶层对象。
///
/// ★ 顶层用「键 → 原始 JSON 文本」而不是 `serde_json::Value`：后者会把所有字段解码
/// 成具体类型再重新编码（数字被转成 f64 可能丢精度、字段顺序被打乱），而本次改写
/// **只**该动 `messages`。用 [`RawValue`] 让其余字段（model / stream / tools /
/// metadata …）**原样字节透传**，与同族的 `forward::rewrite_model` 的既有做法一致。
///
/// ★ 非对象 body（`null` / `[]` / `"a string"` / 非法 JSON）在这里一律返回 `None`，
/// 调用方原样返回请求体。Go 侧的对应守卫更绕：字面量 `null` 能「成功」解析进
/// `map[string]json.RawMessage` 却留下 nil map，随后 `doc["messages"] = ...` 会 panic
/// `assignment to entry in nil map` —— 而 `rewrite` 在转发关键路径上，一次 panic
/// 就是一次请求失败（甚至进程级崩溃）。Rust 侧 `BTreeMap` 的反序列化对 `null` 直接
/// 报错，天然安全，但测试里仍然固化了这一条（防止将来换成 `Option<Map<..>>` 时退化）。
fn parse_doc(body: &[u8]) -> Option<Doc> {
    serde_json::from_slice::<Doc>(body).ok()
}

/// 取出 `messages` 数组。
///
/// `messages` 缺席、为 `null`、或不是数组（对象/字符串）时返回空 vec，调用方据此
/// 直接产出「单条 system」，其余字段照常保留。
fn parse_messages(doc: &Doc) -> Vec<Box<RawValue>> {
    doc.get("messages")
        .and_then(|raw| serde_json::from_str::<Vec<Box<RawValue>>>(raw.get()).ok())
        .unwrap_or_default()
}

/// 把若干段**已是合法 JSON** 的文本拼成一个数组。
///
/// 手工拼接而非走 `Serialize`：元素本身就是原始 JSON 文本，包一层 `Vec<String>` 会
/// 把它们当字符串再转义一遍。这里只加逗号与方括号，元素内容逐字不变。
fn encode_array(items: &[String]) -> Option<String> {
    let mut out = String::with_capacity(items.iter().map(|s| s.len() + 1).sum::<usize>() + 2);
    out.push('[');
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(it);
    }
    out.push(']');
    Some(out)
}

/// 把「键 → 原始 JSON 文本」重新拼成对象。
///
/// [`Doc`] 是 `BTreeMap` ⇒ 键按字典序输出，与 Go `encoding/json` 对 map 的恒定排序
/// 输出**行为一致**（Go 的 `map[string]json.RawMessage` 也是排序输出），不存在顺序
/// 回归。
fn encode_object(doc: &Doc) -> Option<Vec<u8>> {
    serde_json::to_vec(doc).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Load：内置默认 / 文件覆盖 / fail fast
    // -----------------------------------------------------------------

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wbswg-prompt-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).expect("建临时目录");
        d
    }

    /// 空 file → 内置默认（custom 模式不配 file 时的路径）。
    #[test]
    fn load_empty_file_uses_builtin() {
        let got = load("").expect("load(\"\") 不应报错");
        assert_eq!(
            got, DEFAULT_PROMPT,
            "空 file 应返回内置默认（实际 len={} 期望 len={}）",
            got.len(),
            DEFAULT_PROMPT.len()
        );
        assert!(
            !got.trim().is_empty(),
            "内置默认提示词为空：rewrite 会因空提示词变成空操作"
        );
    }

    /// 非空 file → 文件内容**逐字**覆盖内置默认。
    #[test]
    fn load_file_overrides_builtin() {
        let dir = tmpdir("load-override");
        let fp = dir.join("my-prompt.md");
        let want = "你是一名工程助手。\n\n- 保持简洁。\n";
        std::fs::write(&fp, want).expect("写提示词文件");

        let got = load(fp.to_str().unwrap()).expect("load 不应报错");
        assert_eq!(got, want, "文件内容应逐字覆盖内置默认");
        assert_ne!(got, DEFAULT_PROMPT, "返回了内置默认，文件覆盖未生效");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件不存在必须报错（fail fast），不静默回落默认。
    ///
    /// 用户明确配了文件却读不到，若静默用别的提示词，现象是「配了却像没配」，
    /// 排查成本极高 —— 因此启动阶段就要报错。
    #[test]
    fn load_file_missing_fails_fast() {
        let fp = tmpdir("load-missing").join("not-exist.md");
        let err = load(fp.to_str().unwrap()).expect_err("文件不存在应返回 error（fail fast）");
        let msg = err.to_string();
        assert!(
            msg.contains("prompt.file"),
            "错误信息应指明 prompt.file 字段，便于定位，实际: {msg}"
        );
    }

    /// 路径指向目录（读失败）同样报错。
    #[test]
    fn load_file_is_directory_fails_fast() {
        let dir = tmpdir("load-isdir");
        assert!(
            load(dir.to_str().unwrap()).is_err(),
            "路径是目录应返回 error（fail fast）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件存在但内容为空白 → 报错。
    ///
    /// 空提示词会让 rewrite 变成空操作（等价于 passthrough），用户以为在用自有
    /// 提示词、实际客户端 system 一条没动，属于同一种「静默用错」。
    #[test]
    fn load_empty_content_fails_fast() {
        let dir = tmpdir("load-blank");
        let fp = dir.join("blank.md");
        std::fs::write(&fp, "   \n\t\n").expect("写空白文件");
        assert!(
            load(fp.to_str().unwrap()).is_err(),
            "空白内容应返回 error（fail fast）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// file 只含空白 → 视同未配置，用内置默认。
    #[test]
    fn load_whitespace_file_treated_as_empty() {
        let got = load("   ").expect("空白 file 应视同未配置");
        assert_eq!(got, DEFAULT_PROMPT, "空白 file 应返回内置默认");
    }

    /// 内置默认提示词本身不得引入新指纹。
    ///
    /// 本功能的目的就是消灭 system 来源的指纹误报，若默认提示词里带上客户端/厂商
    /// 身份句，等于把误报从客户端搬到网关自己身上。
    #[test]
    fn default_prompt_is_neutral_no_fingerprint() {
        for banned in [
            "Claude Code",
            "Anthropic",
            "OpenAI",
            "Codex",
            "x-anthropic-billing-header",
            "cc_",
        ] {
            assert!(
                !DEFAULT_PROMPT.contains(banned),
                "内置默认提示词含指纹串 {banned:?}：会引入新的 system 指纹误报"
            );
        }
    }

    /// `DEGRADED` 必须存在且中性（不引入指纹）。
    #[test]
    fn degraded_is_neutral() {
        assert!(!DEGRADED.trim().is_empty(), "DEGRADED 为空");
        for banned in ["Claude", "Anthropic", "OpenAI", "Codex"] {
            assert!(!DEGRADED.contains(banned), "DEGRADED 含指纹串 {banned:?}");
        }
    }

    // -----------------------------------------------------------------
    // 测试辅助
    // -----------------------------------------------------------------

    /// 取出改写过后的 role 序列，供断言 messages 结构。
    fn roles_of(body: &[u8]) -> Vec<String> {
        #[derive(Deserialize)]
        struct M {
            #[serde(default)]
            role: String,
        }
        #[derive(Deserialize)]
        struct D {
            #[serde(default)]
            messages: Vec<M>,
        }
        let d: D = serde_json::from_slice(body)
            .unwrap_or_else(|e| panic!("改写过后的 body 不是合法 JSON: {e}"));
        d.messages.into_iter().map(|m| m.role).collect()
    }

    fn assert_roles(got: &[String], want: &[&str]) {
        assert_eq!(
            got.len(),
            want.len(),
            "role 序列长度不符: 实际 {got:?} 期望 {want:?}"
        );
        for (i, w) in want.iter().enumerate() {
            assert_eq!(got[i], *w, "role[{i}]={:?} 期望 {w:?}（完整序列 {got:?}）", got[i]);
        }
    }

    /// 取出第 `i` 条消息的 `content` 字符串（非字符串 content 会 panic）。
    fn content_of(body: &[u8], i: usize) -> String {
        let v: serde_json::Value = serde_json::from_slice(body).expect("合法 JSON");
        v["messages"][i]["content"]
            .as_str()
            .unwrap_or_else(|| panic!("messages[{i}].content 不是字符串: {}", v["messages"][i]))
            .to_string()
    }

    /// messages 数组长度。
    fn messages_len(body: &[u8]) -> usize {
        let v: serde_json::Value = serde_json::from_slice(body).expect("合法 JSON");
        v["messages"].as_array().map(|a| a.len()).unwrap_or(0)
    }

    /// 顶层字段的原始 JSON 文本。
    fn raw_field(body: &[u8], field: &str) -> Option<String> {
        let doc: Doc = serde_json::from_slice(body).ok()?;
        doc.get(field).map(|v| v.get().to_string())
    }

    // -----------------------------------------------------------------
    // Rewrite：出站改写（关键路径）
    // -----------------------------------------------------------------

    /// 删除 system/developer 并在头部插入自有 system。
    #[test]
    fn rewrite_drops_system_and_developer_and_prepends() {
        // ★ 用 `r#"…"#`（str）而不是 `br#"…"#`（byte string）：byte string 只接受
        // ASCII，而本用例刻意带中文 —— 那正是「提示词与内容里有非 ASCII」的真实场景。
        let input = r#"{"model":"glm-5.2","messages":[{"role":"system","content":"OLD-SYSTEM-FINGERPRINT"},{"role":"developer","content":"OLD-DEVELOPER-FINGERPRINT"},{"role":"user","content":"你好"}]}"#;

        let out = rewrite(input.as_bytes(), "MY-SYS");

        assert_roles(&roles_of(&out), &["system", "user"]);
        let s = String::from_utf8(out.clone()).unwrap();
        assert!(
            !s.contains("OLD-SYSTEM-FINGERPRINT") && !s.contains("OLD-DEVELOPER-FINGERPRINT"),
            "旧 system/developer 内容有残留：{s}"
        );
        assert_eq!(
            content_of(&out, 0),
            "MY-SYS",
            "头部 system 内容应为自有提示词"
        );
    }

    /// role 大小写/空白变体同样要删干净。
    ///
    /// 漏删的后果是客户端指纹仍留在请求里，正好绕过本功能的目的。
    #[test]
    fn rewrite_is_case_insensitive_on_role() {
        let input = r#"{"messages":[{"role":"System","content":"A"},{"role":"DEVELOPER","content":"B"},{"role":" system ","content":"C"},{"role":"user","content":"keep"}]}"#;

        let out = rewrite(input.as_bytes(), "SYS");
        assert_roles(&roles_of(&out), &["system", "user"]);
        let s = String::from_utf8(out.clone()).unwrap();
        for leak in ["\"A\"", "\"B\"", "\"C\""] {
            assert!(!s.contains(leak), "旧 system 变体有残留 {leak}：{s}");
        }
    }

    /// 只动 messages 层级，其余消息逐字保留（含顺序）。
    #[test]
    fn rewrite_keeps_user_assistant_tool() {
        let input = r#"{"messages":[{"role":"system","content":"old"},{"role":"user","content":"u-content"},{"role":"assistant","content":"a-content"},{"role":"tool","tool_call_id":"t1","content":"tool-result"}]}"#;

        let out = rewrite(input.as_bytes(), "SYS");
        assert_roles(&roles_of(&out), &["system", "user", "assistant", "tool"]);

        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["messages"][1]["content"], "u-content", "user 消息被改动");
        assert_eq!(
            v["messages"][2]["content"], "a-content",
            "assistant 消息被改动"
        );
        assert_eq!(v["messages"][3]["tool_call_id"], "t1", "tool 消息被改动");
        assert_eq!(v["messages"][3]["content"], "tool-result", "tool 消息被改动");
    }

    /// messages 之外的字段逐字不动。
    ///
    /// 用 RawValue 透传的意义：数字不被转成 f64、字段顺序不变。
    #[test]
    fn rewrite_keeps_other_top_level_fields() {
        let input = r#"{"model":"glm-5.2","stream":true,"max_tokens":100,"temperature":0.7,"metadata":{"conversation_id":"c1"},"tools":[{"type":"function"}],"messages":[{"role":"user","content":"hi"}]}"#;

        let out = rewrite(input.as_bytes(), "SYS");
        // 逐字比较：这些字段必须与原请求**字节一致**。
        for field in [
            "model",
            "stream",
            "max_tokens",
            "temperature",
            "metadata",
            "tools",
        ] {
            assert_eq!(
                raw_field(&out, field),
                raw_field(input.as_bytes(), field),
                "字段 {field} 被改动"
            );
        }
    }

    /// 无 messages 字段 → 插入单条 system，其余字段保留。
    #[test]
    fn rewrite_no_messages_field_inserts_system_only() {
        let input = r#"{"model":"glm-5.2","stream":true}"#;

        let out = rewrite(input.as_bytes(), "SYS");
        assert_roles(&roles_of(&out), &["system"]);
        assert_eq!(raw_field(&out, "model").as_deref(), Some("\"glm-5.2\""));
        assert_eq!(raw_field(&out, "stream").as_deref(), Some("true"));
    }

    /// messages 类型不符（非数组）同样只插 system。
    #[test]
    fn rewrite_bad_messages_type_inserts_system_only() {
        for input in [
            r#"{"messages":"not-an-array","model":"m"}"#,
            r#"{"messages":{"role":"system"},"model":"m"}"#,
            r#"{"messages":null,"model":"m"}"#,
        ] {
            let out = rewrite(input.as_bytes(), "SYS");
            assert_roles(&roles_of(&out), &["system"]);
            assert!(
                String::from_utf8(out.clone()).unwrap().contains("\"m\""),
                "其余字段应保留: in={input} out={}",
                String::from_utf8_lossy(&out)
            );
        }
    }

    /// 非法 JSON 原样返回（绝不失败）。
    ///
    /// rewrite 在转发关键路径上：解析失败该让上游按原语义处理，而不是由网关引入
    /// 一个新的失败原因。
    #[test]
    fn rewrite_invalid_json_returned_as_is() {
        for input in [r#"{not valid json"#, r#"[]"#, r#""a string""#, r#"null"#] {
            let out = rewrite(input.as_bytes(), "SYS");
            assert_eq!(
                String::from_utf8_lossy(&out),
                input,
                "非法/非对象 JSON 应原样返回"
            );
        }
    }

    /// 空 body / 空提示词一律不改写。
    #[test]
    fn rewrite_empty_inputs_returned_as_is() {
        assert!(rewrite(b"", "SYS").is_empty(), "空 body 应原样返回");
        let input = r#"{"messages":[{"role":"system","content":"old"}]}"#;
        assert_eq!(
            rewrite(input.as_bytes(), ""),
            input.as_bytes().to_vec(),
            "空提示词应原样返回（不改写）"
        );
        assert_eq!(
            rewrite(input.as_bytes(), "   "),
            input.as_bytes().to_vec(),
            "空白提示词应原样返回（不改写）"
        );
    }

    /// 提示词含引号/换行/反斜杠时仍产出合法 JSON。
    ///
    /// 提示词来自用户文件，内容不可预期；手工拼 JSON 会拼出非法转义。
    #[test]
    fn rewrite_escapes_prompt_safely() {
        let tricky = "行1 \"带引号\" 与反斜杠 \\ 以及\n换行\t制表";
        let input = r#"{"messages":[{"role":"system","content":"old"},{"role":"user","content":"hi"}]}"#;

        let out = rewrite(input.as_bytes(), tricky);
        assert_eq!(
            content_of(&out, 0),
            tricky,
            "提示词往返不一致（body={}）",
            String::from_utf8_lossy(&out)
        );
    }

    /// 多模态 user content 内部结构不动。
    #[test]
    fn rewrite_multimodal_content_untouched() {
        let input = r#"{"messages":[{"role":"system","content":"old"},{"role":"user","content":[{"type":"text","text":"看图"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAA"}}]}]}"#;

        let out = rewrite(input.as_bytes(), "SYS");
        assert_eq!(messages_len(&out), 2, "messages 长度应为 2");

        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let parts = v["messages"][1]["content"]
            .as_array()
            .unwrap_or_else(|| panic!("多模态 content 不是数组: {}", v["messages"][1]["content"]));
        assert_eq!(parts.len(), 2, "多模态 content 被改动");
        assert_eq!(parts[0]["type"], "text", "text part 被改动");
        assert_eq!(parts[0]["text"], "看图", "text part 被改动");
        assert_eq!(parts[1]["type"], "image_url", "image part 被改动");
    }

    /// 非对象消息（畸形项）保守保留，不误删用户内容。
    #[test]
    fn rewrite_keeps_non_object_messages() {
        let input = r#"{"messages":[{"role":"system","content":"old"},"just-a-string",123]}"#;
        let out = rewrite(input.as_bytes(), "SYS");
        assert_eq!(
            messages_len(&out),
            3,
            "畸形项应被保留（消息数 3），实际: {}",
            String::from_utf8_lossy(&out)
        );
    }

    /// 全部是 system 时只剩一条自有 system。
    #[test]
    fn rewrite_only_system_messages_leaves_single_system() {
        let input = r#"{"messages":[{"role":"system","content":"a"},{"role":"system","content":"b"}]}"#;
        let out = rewrite(input.as_bytes(), "SYS");
        assert_roles(&roles_of(&out), &["system"]);
    }

    /// ★ 大整数必须逐字保留（f64 会丢精度）—— RawValue 透传的核心证据。
    #[test]
    fn rewrite_keeps_big_integer_verbatim() {
        let input = r#"{"max_tokens":9007199254740993,"messages":[{"role":"user","content":"hi"}]}"#;
        let out = rewrite(input.as_bytes(), "SYS");
        assert!(
            String::from_utf8_lossy(&out).contains("9007199254740993"),
            "大整数必须逐字保留（f64 会丢精度），实际: {}",
            String::from_utf8_lossy(&out)
        );
    }

    // -----------------------------------------------------------------
    // Append：追加模式（三条不变式）
    // -----------------------------------------------------------------
    //
    //	① 客户端原有 system **逐字保留**（不能被改写或丢弃）
    //	② 网关提示词插在**开头连续的** system 之后（不是末尾、不是中间）
    //	③ 除 messages 外的字段**字节透传**（与 rewrite 同一纪律）

    /// 客户端的 system 必须逐字保留。
    ///
    /// 这是 append 相对 custom 的**全部意义** —— 丢了它，append 就退化成了一个
    /// 更绕的 custom。
    #[test]
    fn append_keeps_client_system() {
        let body = r#"{"model":"m","messages":[{"role":"system","content":"PROJECT RULES: 必须用 tabs 缩进，不要改 config.go"},{"role":"user","content":"改一下登录逻辑"}]}"#;

        let out = append(body.as_bytes(), "你是网关注入的提示词");
        assert_eq!(
            messages_len(&out),
            3,
            "应为 3 条消息（客户端 system + 网关 system + user）：{}",
            String::from_utf8_lossy(&out)
        );
        // ① 客户端 system 原样在**第一位**
        let roles = roles_of(&out);
        assert_eq!(roles[0], "system", "第 1 条应是客户端的 system");
        assert!(
            content_of(&out, 0).contains("必须用 tabs 缩进"),
            "客户端的项目规范必须逐字保留（append 的全部意义），实际 {:?}",
            content_of(&out, 0)
        );
        // ② 网关提示词紧随其后
        assert_eq!(roles[1], "system", "第 2 条应是网关注入的 system");
        assert_eq!(
            content_of(&out, 1),
            "你是网关注入的提示词",
            "第 2 条应是网关提示词"
        );
        // ③ user 消息未被改动
        assert_eq!(
            content_of(&out, 2),
            "改一下登录逻辑",
            "user 消息不该被改动"
        );
    }

    /// 只跳过**开头连续的** system。
    ///
    /// # 为什么不能「跳到最后一个 system 之后」
    ///
    /// 有些 agentic 客户端会在**对话中途**注入 system（例如工具调用后补一条约束）。
    /// 若把网关提示词插到最后一条 system 之后，等于在「用户说到一半时改了系统规则」
    /// —— 语义上很怪，且会破坏客户端原有的前缀缓存布局。
    #[test]
    fn append_inserts_after_leading_systems_only() {
        let body = r#"{"model":"m","messages":[{"role":"system","content":"规则 A"},{"role":"system","content":"规则 B"},{"role":"user","content":"问题"},{"role":"system","content":"中途注入的规则"},{"role":"assistant","content":"回答"}]}"#;

        let out = append(body.as_bytes(), "网关提示词");
        assert_eq!(
            messages_len(&out),
            6,
            "应为 6 条：{}",
            String::from_utf8_lossy(&out)
        );
        // 第 3 位（索引 2）应是网关提示词 —— 即「开头两条 system 之后」
        assert_eq!(
            content_of(&out, 2),
            "网关提示词",
            "网关提示词应插在开头连续的 system 之后（索引 2），实际索引 2 是 {:?}",
            content_of(&out, 2)
        );
        // 中途那条 system 必须原样留在原位（索引 4）
        assert_eq!(
            content_of(&out, 4),
            "中途注入的规则",
            "对话中途的 system 必须保持原位，实际索引 4 是 {:?}",
            content_of(&out, 4)
        );
    }

    /// 没有 system 时，网关提示词在最前。
    #[test]
    fn append_with_no_system_puts_gateway_first() {
        let body = r#"{"model":"m","messages":[{"role":"user","content":"你好"}]}"#;
        let out = append(body.as_bytes(), "网关提示词");
        assert_eq!(messages_len(&out), 2, "应为 2 条");
        assert_eq!(
            content_of(&out, 0),
            "网关提示词",
            "没有客户端 system 时网关提示词应在最前"
        );
    }

    /// 全是 system 时追加到末尾。
    ///
    /// 边界情况：没有「非 system」可以插在它前面，故只能追加到末尾。不处理会让网关
    /// 提示词**静默不插入**（配置看起来生效了、实际没有）。
    #[test]
    fn append_all_system_messages_appends_at_end() {
        let body = r#"{"model":"m","messages":[{"role":"system","content":"只有规则"}]}"#;
        let out = append(body.as_bytes(), "网关提示词");
        assert_eq!(
            messages_len(&out),
            2,
            "应为 2 条：{}",
            String::from_utf8_lossy(&out)
        );
        assert_eq!(
            content_of(&out, 1),
            "网关提示词",
            "全是 system 时应追加到末尾，实际末条是 {:?}",
            content_of(&out, 1)
        );
    }

    /// developer 也算 system 类。
    ///
    /// 与 `is_system_like` 同一口径：客户端拼写变体不该让插入位置漂移。
    #[test]
    fn append_developer_role_counts_as_system() {
        let body = r#"{"model":"m","messages":[{"role":"developer","content":"开发规范"},{"role":"user","content":"hi"}]}"#;
        let out = append(body.as_bytes(), "网关提示词");
        assert_eq!(
            content_of(&out, 1),
            "网关提示词",
            "developer 应被当作 system 类（提示词插在其后），实际索引 1 是 {:?}",
            content_of(&out, 1)
        );
    }

    /// 除 messages 外字段**字节透传**。
    ///
    /// 与 rewrite 同一纪律：用「键 → 原始 JSON 文本」而非 `Value`，否则数字会被转成
    /// f64 丢精度、字段顺序被打乱。
    #[test]
    fn append_preserves_other_fields() {
        // 用一个大整数，f64 会丢精度（9007199254740993 → ...992）
        let body = r#"{"model":"m","max_tokens":9007199254740993,"temperature":0.7,"messages":[{"role":"user","content":"hi"}]}"#;
        let out = append(body.as_bytes(), "网关提示词");
        let s = String::from_utf8_lossy(&out).to_string();

        assert!(
            s.contains("9007199254740993"),
            "大整数必须逐字保留（f64 会丢精度），实际：{s}"
        );
        assert!(
            s.contains("\"temperature\":0.7"),
            "其它字段应原样保留，实际：{s}"
        );
    }

    /// 空提示词 = 不改写。
    #[test]
    fn append_empty_prompt_is_noop() {
        let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        for p in ["", "   ", "\n\t"] {
            let out = append(body.as_bytes(), p);
            assert_eq!(
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(body.as_bytes()),
                "空提示词 {p:?} 应原样返回"
            );
        }
    }

    /// 非法请求体原样返回（绝不失败）。
    ///
    /// append 在转发关键路径上：宁可少做一次改写，也不能把请求改坏或让请求失败。
    #[test]
    fn append_invalid_body_is_noop() {
        for body in [
            "not json",
            "null",
            "[]",
            r#"{"messages":"不是数组"}"#,
        ] {
            let out = append(body.as_bytes(), "网关提示词");
            if body == r#"{"messages":"不是数组"}"# {
                // 这一种是合法 JSON：会被改写，但**不能 panic**。
                assert_roles(&roles_of(&out), &["system"]);
                continue;
            }
            assert_eq!(
                String::from_utf8_lossy(&out),
                body,
                "{body:?} 应原样返回（不 panic、不改坏）"
            );
        }
    }
}
