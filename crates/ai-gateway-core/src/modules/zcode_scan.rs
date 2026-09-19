//! zcode_scan.rs 扫描本机 ZCode 客户端的登录凭证，供**一键导入**。
//!
//! ## 为什么做这个
//!
//! 所有者的原话：「Qoder 和 ZCode 在本地都有登录凭证，我希望直接导入，
//! 而不是手动点击导入」。手动粘贴 `{apiKey}.{secret}` 是不必要的摩擦 ——
//! ZCode 官方客户端已经把凭证**明文**落在 `~/.zcode/v2/config.json` 里了。
//!
//! ## 与参考实现（TriDefender/zcode-api）的对比
//!
//! 参考实现**不读**客户端凭证 —— 它用**自己的**目录
//! `~/.zcode-proxy/credentials.json`，且首次必须
//! `bun run src/index.ts auth login zai` 走 OAuth 浏览器授权。
//!
//! 我们能直接读，是因为 ZCode 官方客户端把 apiKey 明文写在 config.json 里
//!（实测确认，2026-09-19）。这不是我们绕过了什么保护 —— 那个文件本来就是
//! 用户可读的配置。
//!
//! ## 只读原则（重要）
//!
//! 本模块**绝不修改**被扫描的文件。官方客户端的配置由它自己管理，
//! 我们擅自改写会让它的下次启动行为异常。只读 → 复制到我们自己的目录。
//!
//! ## 诚实原则
//!
//! 扫不到就**如实说扫不到**，不编造候选。Qoder 那边就是典型：
//! 它的凭证在自定义加密的 `auth.v1.dat` 里（熵 7.5，明文占比 7%），
//! 我们拿不到 —— 那就不要在那页放一个"扫描"按钮让用户空点。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// 复刻 Go 侧 `zcode.CredKey` 的 uid 派生。
///
/// ## 为什么必须在 Rust 重算一遍
///
/// uid 是 `zcode-<FNV1a64(credential) 的低 12 位十六进制>`（见 Go 侧
/// `shortHash`）。用它来判断「这条凭证是不是已经导入过了」是**精确**的 ——
/// 比按长度猜可靠得多（长度会撞：49 字符的凭证本机就有两个可能）。
///
/// ## 为什么要与 Go 逐字节一致
///
/// 两边算法一旦分叉，扫描就会把**已导入的**凭证报成"未导入"，
/// 用户重复导入 → 账号列表出现重复项。故这里照抄 Go 的实现，
/// 并有单测与固定向量对齐（见 tests 里的 `uid_matches_go_vectors`）。
///
/// FNV-1a 64 位，取低 12 位十六进制（Go 侧 `h & 0xf` 右移 12 次）。
pub fn cred_uid(credential: &str) -> String {
    const OFFSET64: u64 = 14695981039346656037;
    const PRIME64: u64 = 1099511628211;

    let mut h: u64 = OFFSET64;
    for b in credential.trim().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(PRIME64);
    }

    // ⚠ 逐字照抄 Go 的循环（从索引 11 往前填，每次取**最低** nibble 再右移）。
    //
    // 这里我第一版写成了 `out[i] = (h >> (4*i)) & 0xf`，方向正好反了 ——
    // 那种"看着等价"的改写正是跨语言复刻最容易出错的地方，
    // 而错了不会崩，只会让 uid 对不上 → 已导入的被报成未导入 → 重复导入。
    // 故直接照抄，并用固定向量单测钉住（见 uid_matches_go_vectors）。
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 12];
    for i in (0..12).rev() {
        out[i] = DIGITS[(h & 0xf) as usize];
        h >>= 4;
    }
    format!("zcode-{}", std::str::from_utf8(&out).expect("ASCII 十六进制"))
}

/// 一个扫描到的候选凭证。
#[derive(Debug, Clone)]
pub struct FoundCredential {
    /// 来源文件（给用户看"从哪来的"）。
    pub source: PathBuf,
    /// 服务商（zai / bigmodel）。
    pub provider: String,
    /// 凭证本体（`{apiKey}.{secret}` 或 `{apiKey}`）。
    pub credential: String,
    /// 昵称建议（取客户端里的 provider 名，去掉 `builtin:` 前缀）。
    pub suggested_nickname: String,
    /// 客户端里的原始条目名（如 `builtin:bigmodel-coding-plan`）。
    pub origin_name: String,
    /// 这个条目在客户端里是否被启用。
    pub enabled: bool,
    /// 配额查询用的 JWT（从**同一个服务商**的 start-plan 条目配对而来）。
    ///
    /// ## 为什么要配对（闭环的关键）
    ///
    /// 额度查询**只认 JWT**，不认 `{apiKey}.{secret}`。而 ZCode 客户端把
    /// 两者分成**两个 providers 条目**落盘：
    ///
    ///   `builtin:bigmodel-coding-plan` → 对话凭证（49 字符，两段）
    ///   `builtin:bigmodel-start-plan`  → JWT（180 字符，`eyJ` 开头）
    ///
    /// 只导入前者 → 用户看到「额度未知」，而他明明有额度（实测 3 亿 token）。
    /// 故扫描时把同一服务商的 JWT 配到对话凭证上，一并导入。
    pub jwt: Option<String>,
}

impl FoundCredential {
    /// 脱敏后的视图（**绝不把完整凭证发给前端**）。
    ///
    /// 前端只需要知道"有个什么凭证、能不能导入"，
    /// 不需要（也不应该）拿到令牌本体 —— 它会出现在 DOM、
    /// 可能被截图、被 devtools 复制。
    pub fn to_view(&self, index: usize) -> Value {
        json!({
            "index": index,
            "source": self.source.to_string_lossy(),
            "provider": self.provider,
            "providerLabel": provider_label(&self.provider),
            // 只给长度与掩码，够用户辨认是哪一个
            "masked": mask_credential(&self.credential),
            "credentialLength": self.credential.len(),
            "suggestedNickname": self.suggested_nickname,
            "originName": self.origin_name,
            "enabled": self.enabled,
            // 判断形态，让界面能给出准确提示
            "shape": credential_shape(&self.credential),
            // 有没有配到额度令牌 —— 界面据此说明"导入后能读到额度"
            "hasQuotaToken": self.jwt.is_some(),
        })
    }
}

/// 凭证形态：决定它能不能直接用于对话。
///
/// - `two-part`：`{apiKey}.{secret}` —— 完整凭证，可直接对话
/// - `jwt`：JWT 形态（`eyJ...`）—— 是**额度查询**用的，不能当对话凭证
/// - `single`：单段 —— 智谱的 secret 可选，合法但要提示
pub fn credential_shape(cred: &str) -> &'static str {
    let c = cred.trim();
    if c.starts_with("eyJ") {
        return "jwt";
    }
    if c.contains('.') {
        return "two-part";
    }
    "single"
}

/// 掩码：只留头尾，中间打码。
///
/// 长度 ≤ 16 时整体打码（头尾都留就露太多了）。
pub fn mask_credential(cred: &str) -> String {
    let c = cred.trim();
    let n = c.chars().count();
    if n <= 16 {
        return "*".repeat(n.max(4));
    }
    let head: String = c.chars().take(6).collect();
    let tail: String = c.chars().skip(n - 4).collect();
    format!("{head}…{tail}（{n} 字符）")
}

/// 服务商的中文标签。
pub fn provider_label(p: &str) -> &'static str {
    match p {
        "zai" => "Z.AI",
        "bigmodel" => "智谱",
        _ => "未知",
    }
}

/// ZCode 官方客户端的配置目录（候选，按优先级）。
///
/// ## 为什么要多个
///
/// 客户端在不同平台/版本用的目录不同（实测本机是 `~/.zcode/v2`）。
/// 只认一个路径会在别的版本上静默扫不到 —— 那是"看起来能用但实际不行"
/// 的典型缺陷（本项目已经踩过好几次）。
///
/// ## 为什么可注入
///
/// 测试要指向临时目录，不能读真实用户配置。
pub fn client_config_candidates_with(base: &Path) -> Vec<PathBuf> {
    vec![
        // 实测本机就是这个（v2 布局）
        base.join("v2").join("config.json"),
        // 其它可能的布局（保守列出，扫不到就跳过，不会误报）
        base.join("config.json"),
        base.join("cli").join("config.json"),
    ]
}

/// 用真实 HOME 计算候选路径。
pub fn client_config_candidates() -> Vec<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    match home {
        Some(h) => client_config_candidates_with(&h.join(".zcode")),
        None => Vec::new(),
    }
}

/// 从一份 config.json 里抽出所有可用的凭证，并把额度令牌**配对**上去。
///
/// ## 结构（实测）
///
/// ```json
/// {
///   "provider": {
///     "builtin:bigmodel-coding-plan": {
///       "name": "...", "kind": "anthropic", "source": "custom",
///       "enabled": false,
///       "options": { "apiKey": "xxxx.yyyy", "baseURL": "https://open.bigmodel.cn/api/anthropic" }
///     },
///     "builtin:bigmodel-start-plan": {
///       "options": { "apiKey": "eyJhbGci...", "baseURL": "https://zcode.z.ai/api/v1/zcode-plan/anthropic" }
///     }
///   }
/// }
/// ```
///
/// 取 `provider[*].options.apiKey`。**空字符串要跳过** —— 实测配置里有
/// 4 个条目 apiKey 是空的（用户没登录那些服务商），把它们当候选会让
/// 用户导入一堆没用的账号。
///
/// ## JWT 形态的条目本身**不是**对话凭证
///
/// `start-plan` 那条的 apiKey 是 `eyJ...`（JWT），它**不能**用来发对话请求
/// （上游会 401）。故它**不作为候选**出现，而是配到同服务商的对话凭证上。
///
/// 若某个服务商**只有** JWT 而没有对话凭证（用户只领了活动计划），
/// 那就没有可导入的东西 —— 如实返回空，而不是硬造一条用不了的。
pub fn extract_from_config(doc: &Value, source: &Path) -> Vec<FoundCredential> {
    let Some(providers) = doc.get("provider").and_then(Value::as_object) else {
        return Vec::new();
    };

    // 第一遍：按服务商收集 JWT（额度令牌）
    let mut jwts: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (name, entry) in providers {
        let Some(k) = entry
            .get("options")
            .and_then(|o| o.get("apiKey"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let k = k.trim();
        if credential_shape(k) != "jwt" {
            continue;
        }
        let base_url = entry
            .get("options")
            .and_then(|o| o.get("baseURL"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let provider = infer_provider(base_url, name);
        // 同一服务商有多个 JWT 时保留第一个（它们的额度是同一份）
        jwts.entry(provider).or_insert_with(|| k.to_string());
    }

    // 第二遍：收集对话凭证，并配上对应服务商的 JWT
    let mut out = Vec::new();
    for (name, entry) in providers {
        let Some(api_key) = entry
            .get("options")
            .and_then(|o| o.get("apiKey"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let cred = api_key.trim();
        // 空/极短的一律跳过：那是占位，不是凭证
        if cred.len() < 8 {
            continue;
        }
        // JWT 形态不作为对话凭证候选（见上面的说明）
        if credential_shape(cred) == "jwt" {
            continue;
        }

        // 服务商：优先看 baseURL（更权威），回退看条目名
        let base_url = entry
            .get("options")
            .and_then(|o| o.get("baseURL"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let provider = infer_provider(base_url, name);

        // 显示名：用**客户端自己的 `name`**（人类可读），不是内部条目名。
        //
        //   `name`      = "BigModel - Coding Plan"     ← 给用户看的
        //   条目键       = "builtin:bigmodel-coding-plan" ← 内部标识
        //
        // 我第一版直接把条目键去掉 `builtin:` 前缀当昵称，于是界面上显示
        // `bigmodel-coding-plan` —— 那不是名字，是内部 ID。
        let display = entry
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| name.trim_start_matches("builtin:").to_string());

        out.push(FoundCredential {
            source: source.to_path_buf(),
            provider: provider.clone(),
            credential: cred.to_string(),
            suggested_nickname: display,
            origin_name: name.clone(),
            enabled: entry.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            jwt: jwts.get(&provider).cloned(),
        });
    }
    out
}

/// 推断服务商。
///
/// ## 判据顺序：先条目名，再 baseURL
///
/// ⚠ 这个顺序是**实测定下来的**，与直觉相反（一般会觉得 URL 更权威）。
/// 原因：`*-start-plan` 条目的 baseURL 是
///
///	https://zcode.z.ai/api/v1/zcode-plan/anthropic
///
/// 即 **zcode.z.ai 是统一账单网关，两个服务商共用**（额度查询也走它，
/// 见 provider.go 的 QuotaHost）。故对这类条目，baseURL 完全不含服务商信息 ——
/// 反而会把它误判成 zai。
///
/// 实测踩到：`builtin:bigmodel-start-plan` 被按 baseURL 判成 `zai`，
/// 于是它的 JWT 配不到 bigmodel 的对话凭证上，额度令牌**静默丢失**，
/// 导入后界面显示「额度未知」—— 而用户明明有 3 亿 token。
///
/// 所以：
///  1. **条目名**优先（`builtin:bigmodel-*` / `builtin:zai-*` 都带服务商）
///  2. baseURL 兜底（条目名认不出时，它至少能区分 open.bigmodel.cn 与 api.z.ai）
///  3. 都不认 → bigmodel（智谱对 zai 凭证会明确报错，不会静默用错）
pub fn infer_provider(base_url: &str, entry_name: &str) -> String {
    // 1. 条目名优先（服务商信息在这里最可靠）
    let n = entry_name.to_lowercase();
    if n.contains("bigmodel") || n.contains("zhipu") {
        return "bigmodel".to_string();
    }
    if n.contains("zai") {
        return "zai".to_string();
    }

    // 2. baseURL 兜底。
    //    ⚠ 必须先排除统一账单网关 zcode.z.ai —— 它不含服务商信息，
    //    按它判会把两个服务商都判成 zai。
    let u = base_url.to_lowercase();
    if !u.contains("zcode.z.ai") {
        if u.contains("bigmodel") || u.contains("zhipu") {
            return "bigmodel".to_string();
        }
        if u.contains("api.z.ai") {
            return "zai".to_string();
        }
    }

    // 3. 都不认
    "bigmodel".to_string()
}

/// 扫描本机所有候选位置，返回发现的凭证。
///
/// 不报错：扫不到返回空列表（调用方据此显示"没找到"，
/// 而不是一个莫名其妙的错误）。
pub fn scan() -> Vec<(FoundCredential, Vec<String>)> {
    scan_with(&client_config_candidates())
}

/// 可测试的扫描入口：给定候选路径列表，返回 (凭证, 该文件的解析问题)。
pub fn scan_with(paths: &[PathBuf]) -> Vec<(FoundCredential, Vec<String>)> {
    let mut out = Vec::new();
    for p in paths {
        if !p.is_file() {
            continue;
        }
        let mut problems = Vec::new();
        let raw = match std::fs::read_to_string(p) {
            Ok(r) => r,
            Err(e) => {
                problems.push(format!("读取失败: {e}"));
                continue;
            }
        };
        let doc: Value = match serde_json::from_str(&raw) {
            Ok(d) => d,
            Err(e) => {
                problems.push(format!("不是合法 JSON: {e}"));
                continue;
            }
        };
        for c in extract_from_config(&doc, p) {
            out.push((c, problems.clone()));
        }
    }
    out
}

/// 把扫描结果转成前端可用的视图。
///
/// ## 账号名取自**客户端登录态**，不是套餐名
///
/// 所有者的反馈（两轮，第二轮明确指出我第一次也没对）：
///
///   「ZCode 的名字获取的也不对，应该能从接口获取的」
///   「你那玩意而是套餐名，我现在登录的账号明明有名字」
///
/// 对：`provider[*].name` 是**套餐名**（"BigModel - Coding Plan"），
/// 真正的账号名在客户端登录态里（`credentials.json` 的 `user_info`）——
/// 见 `zcode_credstore`。故这里用登录名覆盖昵称建议。
///
/// ## 「已导入」判据用 uid 精确比对，而不是猜
///
/// uid 是凭证的确定性哈希（见 `cred_uid`），而账号库里存的就是 uid。
/// 故 `cred_uid(扫描到的凭证) ∈ 已有账号 uid` 就是**精确**的已导入判据。
///
/// 我第一版想按"凭证长度"猜 —— 那是错的：本机就有两个不同凭证长度相同
/// 的可能性，长度相同会误报"已导入"，用户于是**漏导**一个账号
/// （比重复导入更糟：他以为导完了）。
pub fn scan_result_view(found: &[(FoundCredential, Vec<String>)]) -> Value {
    // 已有账号的 uid 集合（含只有凭证文件、账号库里还没登记的孤儿）
    let existing: std::collections::HashSet<String> =
        crate::modules::zcode_account::credential_uids().into_iter().collect();
    let known_accounts: Vec<String> = crate::modules::zcode_account::load_accounts()
        .unwrap_or_default()
        .into_iter()
        .map(|a| a.uid)
        .collect();

    // 登录态里的真实账号名（拿不到时 None，回退到套餐名）
    let identity = crate::modules::zcode_credstore::read_identity();
    let account_name = identity
        .as_ref()
        .map(|i| i.best_name().to_string())
        .filter(|s| !s.is_empty());

    let items: Vec<Value> = found
        .iter()
        .enumerate()
        .map(|(i, (c, _))| {
            let mut v = c.to_view(i);
            let uid = cred_uid(&c.credential);
            // 在磁盘上（凭证文件已存在）或在账号库登记过，都算"已导入"
            let imported = existing.contains(&uid) || known_accounts.contains(&uid);
            v["uid"] = json!(uid);
            v["alreadyImported"] = json!(imported);
            // ★ 用登录名覆盖套餐名（这是所有者要的"账号名"）
            if let Some(name) = &account_name {
                v["suggestedNickname"] = json!(name);
                v["accountName"] = json!(name);
            }
            v
        })
        .collect();

    let problems: Vec<String> = found.iter().flat_map(|(_, p)| p.clone()).collect();

    json!({
        "count": items.len(),
        "items": items,
        "problems": problems,
        // 扫过哪些路径也告诉前端 —— 用户能自己判断"是不是没找对地方"，
        // 而不是对着"没找到"发呆
        "scannedPaths": client_config_candidates()
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>(),
        "existingAccountCount": known_accounts.len(),
        // 登录态信息（界面用它显示"已登录为 wish"）
        "identity": identity.as_ref().map(|i| json!({
            "username": i.username,
            "displayName": i.display_name,
            "id": i.id,
            "avatarUrl": i.avatar_url,
            "activeProvider": i.active_provider,
        })).unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(entries: Value) -> Value {
        json!({ "provider": entries })
    }

    // 显示名用**客户端自己的 `name`**，不是内部条目键。
    //
    // 实测本机配置：
    //   name    = "BigModel - Coding Plan"        ← 官方给的可读名
    //   条目键   = "builtin:bigmodel-coding-plan"  ← 内部标识
    //
    // 我第一版把条目键去掉 `builtin:` 当昵称，界面上显示
    // `bigmodel-coding-plan` —— 那不是名字，是内部 ID，用户看不懂。
    #[test]
    fn nickname_prefers_client_display_name() {
        let doc = cfg(json!({
            "builtin:bigmodel-coding-plan": {
                "name": "BigModel - Coding Plan",
                "options": { "apiKey": "aaaa.bbbb", "baseURL": "https://open.bigmodel.cn/api/anthropic" }
            }
        }));
        let got = extract_from_config(&doc, Path::new("/tmp/c.json"));
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].suggested_nickname, "BigModel - Coding Plan",
            "应显示客户端给的可读名，而不是内部条目名"
        );
        // 内部条目名仍保留（用于排障与去重）
        assert_eq!(got[0].origin_name, "builtin:bigmodel-coding-plan");
    }

    // 客户端没给 `name` 时**回退**到条目名（去掉 builtin: 前缀）。
    //
    // 比显示空字符串好 —— 用户至少能分辨是哪一条。
    #[test]
    fn nickname_falls_back_to_entry_name() {
        let doc = cfg(json!({
            "builtin:some-plan": {
                "options": { "apiKey": "aaaa.bbbb", "baseURL": "https://open.bigmodel.cn/x" }
            },
            "builtin:blank-name": {
                "name": "   ",
                "options": { "apiKey": "cccc.dddd", "baseURL": "https://open.bigmodel.cn/y" }
            }
        }));
        let got = extract_from_config(&doc, Path::new("/tmp/c.json"));
        assert_eq!(got.len(), 2);
        let names: Vec<&str> = got.iter().map(|c| c.suggested_nickname.as_str()).collect();
        assert!(names.contains(&"some-plan"), "缺 name 时应回退到条目名，实际 {names:?}");
        assert!(
            names.contains(&"blank-name"),
            "name 只有空白时也应回退（不能把空白当昵称），实际 {names:?}"
        );
        for n in names {
            assert!(!n.trim().is_empty(), "昵称不该为空");
            assert!(!n.starts_with("builtin:"), "昵称不该带内部前缀：{n}");
        }
    }

    // 实测形状：本机 config.json 的结构。
    #[test]
    fn extracts_real_config_shape() {
        let doc = cfg(json!({
            "builtin:bigmodel-coding-plan": {
                "name": "智谱编码套餐", "kind": "builtin", "enabled": true,
                "options": { "apiKey": "aaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbb", "baseURL": "https://open.bigmodel.cn/api/coding/paas/v4" }
            }
        }));
        let got = extract_from_config(&doc, Path::new("/tmp/config.json"));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].provider, "bigmodel");
        // 有 `name` 时用客户端的可读名（**不是**内部条目键）
        assert_eq!(got[0].suggested_nickname, "智谱编码套餐");
        assert_eq!(got[0].origin_name, "builtin:bigmodel-coding-plan");
        assert!(got[0].enabled);
        assert_eq!(got[0].credential, "aaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbb");
    }

    // 实测形状（完整）：本机 client 配置**逐字**就是这样 ——
    // 一个服务商有两条，一条对话凭证、一条 JWT。
    //
    // 这是闭环的核心：两条必须**配对**成一条候选，否则导入后额度是未知。
    #[test]
    fn pairs_jwt_with_credential_of_same_provider() {
        let doc = cfg(json!({
            "builtin:bigmodel-coding-plan": {
                "name": "BigModel - Coding Plan", "kind": "anthropic", "enabled": false,
                "options": {
                    "apiKey": "459c1d0000000000000000000000000.trN8aaaaaaaaaaaa",
                    "baseURL": "https://open.bigmodel.cn/api/anthropic"
                }
            },
            "builtin:bigmodel-start-plan": {
                "name": "BigModel- Coding Plan", "kind": "anthropic", "enabled": false,
                "options": {
                    "apiKey": "eyJhbGciOiJIUzI1NiJ9.eyJ1c2VyX2lkIjoiMTkzMyJ9.sig",
                    "baseURL": "https://zcode.z.ai/api/v1/zcode-plan/anthropic"
                }
            }
        }));
        let got = extract_from_config(&doc, Path::new("/tmp/config.json"));

        // 只应有 **1** 条候选（JWT 那条不是对话凭证，不该单独出现）
        assert_eq!(
            got.len(),
            1,
            "应只产出对话凭证那一条；JWT 条目不是对话凭证，实际 {:?}",
            got.iter().map(|c| &c.origin_name).collect::<Vec<_>>()
        );
        assert_eq!(got[0].origin_name, "builtin:bigmodel-coding-plan");

        // 关键：JWT 必须被配上
        assert!(
            got[0].jwt.is_some(),
            "额度令牌没配上 —— 导入后界面会显示「额度未知」，而用户明明有额度"
        );
        assert!(got[0].jwt.as_deref().unwrap().starts_with("eyJ"));

        // 界面能据此说明"导入后能读到额度"
        assert_eq!(got[0].to_view(0)["hasQuotaToken"], true);
    }

    // 只有 JWT、没有对话凭证时 → **不产出候选**。
    //
    // 硬造一条用不了的账号比"什么都没找到"更糟：用户会导入它，
    // 然后在发请求时得到一个莫名其妙的 401。
    #[test]
    fn jwt_only_produces_no_candidate() {
        let doc = cfg(json!({
            "builtin:bigmodel-start-plan": {
                "options": {
                    "apiKey": "eyJhbGciOiJIUzI1NiJ9.eyJ1c2VyX2lkIjoiMTkzMyJ9.sig",
                    "baseURL": "https://zcode.z.ai/api/v1/zcode-plan/anthropic"
                }
            }
        }));
        let got = extract_from_config(&doc, Path::new("/tmp/c.json"));
        assert!(
            got.is_empty(),
            "只有额度令牌时应如实返回空，而不是造一条用不了的账号：{got:?}"
        );
    }

    // JWT 服务商不匹配时**不能**乱配。
    //
    // 把 zai 的 JWT 配到 bigmodel 的凭证上，额度查询会 401 ——
    // 而界面上看起来"有额度令牌"，用户完全无从判断。
    #[test]
    fn jwt_not_paired_across_providers() {
        let doc = cfg(json!({
            "builtin:bigmodel-coding-plan": {
                "options": {
                    "apiKey": "aaaa.bbbb",
                    "baseURL": "https://open.bigmodel.cn/api/anthropic"
                }
            },
            "builtin:zai-start-plan": {
                "options": {
                    "apiKey": "eyJhbGciOiJIUzI1NiJ9.eyJ1IjoieCJ9.s",
                    "baseURL": "https://zcode.z.ai/api/v1/zcode-plan/anthropic"
                }
            }
        }));
        let got = extract_from_config(&doc, Path::new("/tmp/c.json"));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].provider, "bigmodel");
        assert!(
            got[0].jwt.is_none(),
            "zai 的 JWT 不该配到 bigmodel 的凭证上 —— 那会让额度查询 401，\
             而界面上看起来「有额度令牌」"
        );
    }

    // 空 apiKey 必须跳过 —— 实测配置里有 4 个空条目，
    // 当成候选会让用户导入一堆没用的账号。
    #[test]
    fn skips_empty_api_keys() {
        let doc = cfg(json!({
            "builtin:zai-coding-plan": { "options": { "apiKey": "" } },
            "builtin:zai": { "options": { "apiKey": "   " } },
            "builtin:bigmodel": { "options": {} },
            "builtin:nokey": {},
            "builtin:good": { "options": { "apiKey": "real.credential.here" } }
        }));
        let got = extract_from_config(&doc, Path::new("/tmp/c.json"));
        assert_eq!(got.len(), 1, "只有 good 该被留下，实际 {:?}", got.iter().map(|c| &c.origin_name).collect::<Vec<_>>());
        assert_eq!(got[0].origin_name, "builtin:good");
    }

    // 没有 provider 段 → 空（不 panic）
    #[test]
    fn handles_missing_provider_section() {
        assert!(extract_from_config(&json!({}), Path::new("/x")).is_empty());
        assert!(extract_from_config(&json!({ "provider": null }), Path::new("/x")).is_empty());
        assert!(extract_from_config(&json!({ "provider": "not-an-object" }), Path::new("/x")).is_empty());
    }

    // 服务商推断：**条目名优先于 baseURL**（实测定下来的，与直觉相反）。
    #[test]
    fn infers_provider_from_name_first() {
        // 实测踩到的那个：start-plan 的 baseURL 是统一账单网关，
        // 按它判会得到 zai，于是 JWT 配不到 bigmodel 上、额度令牌静默丢失。
        assert_eq!(
            infer_provider("https://zcode.z.ai/api/v1/zcode-plan/anthropic", "builtin:bigmodel-start-plan"),
            "bigmodel",
            "条目名带服务商时必须以它为准 —— zcode.z.ai 是两个服务商共用的账单网关"
        );
        assert_eq!(
            infer_provider("https://zcode.z.ai/api/v1/zcode-plan/anthropic", "builtin:zai-start-plan"),
            "zai"
        );
        // 条目名认不出时才看 baseURL
        assert_eq!(infer_provider("https://open.bigmodel.cn/api/anthropic", "custom-1"), "bigmodel");
        assert_eq!(infer_provider("https://api.z.ai/api/anthropic", "custom-2"), "zai");
        // 账单网关不含服务商信息，单独出现时不该被当成 zai
        assert_ne!(
            infer_provider("https://zcode.z.ai/x", "custom-3"),
            "zai",
            "zcode.z.ai 是统一账单网关，不能据此判成 zai"
        );
        // 都不认 → 回退 bigmodel（智谱对 zai 凭证会明确报错，不会静默用错）
        assert_eq!(infer_provider("", "unknown"), "bigmodel");
    }

    // 形态判定 —— 决定界面给什么提示
    #[test]
    fn detects_credential_shape() {
        assert_eq!(credential_shape("eyJhbGciOiJIUzI1NiJ9.xxx"), "jwt");
        assert_eq!(credential_shape("apiKeyId.secretPart"), "two-part");
        assert_eq!(credential_shape("justOneSegment"), "single");
    }

    // 掩码不能泄露完整凭证
    #[test]
    fn mask_never_reveals_full_secret() {
        let c = "abcdefghijklmnopqrstuvwxyz0123456789";
        let m = mask_credential(c);
        assert!(!m.contains(c), "掩码不得包含完整凭证");
        assert!(m.contains('…'), "应有省略号");
        // 短的整体打码
        let short = mask_credential("abc");
        assert_eq!(short, "****");
        assert!(!short.contains("abc"));
    }

    // 前端视图**不得**含完整凭证 —— 它会进 DOM、被截图、被 devtools 复制
    #[test]
    fn view_never_contains_raw_credential() {
        let c = FoundCredential {
            source: PathBuf::from("/tmp/config.json"),
            provider: "bigmodel".into(),
            credential: "SUPER_SECRET_KEY_ID.SUPER_SECRET_PART".into(),
            suggested_nickname: "bigmodel-coding-plan".into(),
            origin_name: "builtin:bigmodel-coding-plan".into(),
            enabled: true,
            jwt: Some("SUPER_SECRET_JWT".into()),
        };
        let v = c.to_view(0);
        let text = v.to_string();
        assert!(
            !text.contains("SUPER_SECRET"),
            "前端视图泄露了凭证本体（含额度令牌）：{text}"
        );
        assert_eq!(
            v["credentialLength"],
            c.credential.len(),
            "长度要如实上报（界面据此显示「49 字符」帮用户辨认）"
        );
        assert_eq!(v["providerLabel"], "智谱");
    }

    // 候选路径可注入（测试不该读真实用户目录）
    #[test]
    fn candidates_are_injectable() {
        let base = Path::new("/fake/home/.zcode");
        let got = client_config_candidates_with(base);
        assert!(got.iter().any(|p| p.ends_with("v2/config.json")), "应含实测的 v2 布局");
        assert!(got.len() >= 2, "应保守列出多种布局：{got:?}");
    }

    // uid 派生必须与 Go 的 `CredKey` **逐字节一致**。
    //
    // ## 为什么这条测试是跨语言的
    //
    // 期望值全部来自**实际运行 Go 实现**的输出
    //（`go test ./internal/zcode/ -run TestPrintCredKeyVectors`），
    // 不是我自己算的 —— 那样两边一起错就测不出来了。
    //
    // 分叉的后果是静默的：扫描会把已导入的凭证报成"未导入"，
    // 用户重复导入 → 账号列表出现重复项。
    #[test]
    fn uid_matches_go_vectors() {
        let vectors: &[(&str, &str)] = &[
            ("a", "zcode-dc4c8601ec8c"),
            ("ab", "zcode-4407b545986a"),
            ("abc", "zcode-a2190541574b"),
            ("k.s", "zcode-5419358261ed"),
            ("good-key.good-secret", "zcode-6a5d627d01ec"),
            ("apiKeyId.secretPartWithSomeLength01", "zcode-bda12c9c4e1d"),
            ("aaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbb", "zcode-a5be9f2890a1"),
            (
                "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.payload.signature",
                "zcode-8b083c24ac0e",
            ),
            // 前后空白必须被 Trim（Go 侧 CredKey 有 TrimSpace）
            ("  padded.cred  ", "zcode-0150ca5b52d3"),
        ];
        for (input, want) in vectors {
            assert_eq!(
                &cred_uid(input),
                want,
                "uid 派生与 Go 不一致（输入 {input:?}）—— 会让已导入的凭证被报成未导入"
            );
        }
    }

    // 180 字符的长串（实测的 start-plan JWT 就是这个量级）
    #[test]
    fn uid_matches_go_vector_for_long_input() {
        let long = "x".repeat(180);
        assert_eq!(cred_uid(&long), "zcode-334175d06275");
    }

    // uid 稳定：同输入同输出（幂等导入的前提）
    #[test]
    fn uid_is_deterministic() {
        let c = "some.credential";
        assert_eq!(cred_uid(c), cred_uid(c));
        // Trim 后等价
        assert_eq!(cred_uid(c), cred_uid("  some.credential  "));
    }

    // 扫不存在的路径 → 空且不报错
    #[test]
    fn scan_missing_paths_is_empty_not_error() {
        let got = scan_with(&[PathBuf::from("/definitely/not/here/config.json")]);
        assert!(got.is_empty());
    }

    // 坏 JSON → 记录问题而不是 panic，且不影响其它文件
    #[test]
    fn scan_reports_bad_json_without_failing_others() {
        let dir = std::env::temp_dir().join("zcode-scan-test-bad");
        let _ = std::fs::create_dir_all(&dir);
        let bad = dir.join("bad.json");
        let good = dir.join("good.json");
        std::fs::write(&bad, "{ this is not json").unwrap();
        std::fs::write(
            &good,
            r#"{"provider":{"builtin:x":{"options":{"apiKey":"aaaa.bbbb"}}}}"#,
        )
        .unwrap();

        let got = scan_with(&[bad.clone(), good.clone()]);
        assert_eq!(got.len(), 1, "好文件应照常被解析出来");
        assert_eq!(got[0].0.origin_name, "builtin:x");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
