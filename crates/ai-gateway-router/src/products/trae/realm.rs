//! Trae 区域表（`ai-gateway-core::modules::trae_realm` 的**最小等价实现**）。
//!
//! # 为什么 router 侧要自带一份
//!
//! 本 crate **刻意不依赖 `ai-gateway-core`**（见 `crates/ai-gateway-router/Cargo.toml`
//! 文件头：core 会 `include!` build.rs 生成的 `gateway_embed.rs`，把网关二进制 gzip
//! 内嵌进库；而网关又依赖本 crate ⇒ 网关会背一份自己被压缩的副本约 3MB，
//! 并拉进宿主的全部重依赖）。
//!
//! 表内容与 core 的 `trae_realm.rs` **逐项一致**（两者都是 `realms.js` 167 行的移植）
//! ⇒ ★ **改一处必须改两处**，否则「宿主判定国服、网关判定国际版」这类错位会在
//! 运行时表现为「凭证明明是好的，请求却打到错误区域的 host」。
//!
//! # 区域判定必须以**凭据自身**为准
//!
//! 顺序（与 core 同）：① 凭据里显式区域（`_edition`/`realm`）→ ② host/issuer 里的
//! 区域标记 → ③ `userRegion` 区域码 → ④ 兜底 `DEFAULT_REALM`。
//! 前三步都可能拿不到（实盘 `trae_accounts.json` 的 JWT **没有 `iss` 字段**），
//! 所以导出凭证时宿主会把判定结果**显式写进** `auth.realm` —— 那一步之后
//! 网关走的就是第 ① 步，不再依赖猜测。

/// Trae 的两条产品线。
///
/// ★ 命名与 core 一致用 `Sg`（而不是 `Intl`）：Trae 的国际版入口是新加坡节点
/// （`coresg-normal.trae.ai`），另有美国节点（`coreva-normal.trae.ai`）由
/// `userRegion == "US"` 单独选，**不是**第三个 realm。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Realm {
    /// 国内版（`trae-api-cn.mchost.guru`）。
    Cn,
    /// 国际版（`coresg-normal.trae.ai`）。
    Sg,
}

impl Realm {
    pub fn as_str(self) -> &'static str {
        match self {
            Realm::Cn => "cn",
            Realm::Sg => "sg",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Realm::Cn => "Trae 国内版 (China)",
            Realm::Sg => "Trae 国际版 (Global)",
        }
    }

    /// 解析区域标识（大小写不敏感）。
    ///
    /// 别名与 core/`realms.js` 的 `normalizeRealm` 一致：`solo`/`solo-cn`/`solo_cn`
    /// 都归国内版 —— 它们指的是「SOLO 产品形态」，而 SOLO 最早只有国内版，
    /// 历史凭证里就写着这几个值。
    pub fn parse(value: &str) -> Option<Realm> {
        let v = value.trim().to_ascii_lowercase();
        match v.as_str() {
            "solo" | "solo-cn" | "solo_cn" | "cn" => Some(Realm::Cn),
            "sg" => Some(Realm::Sg),
            _ => None,
        }
    }
}

/// 一个区域的静态配置（`realms.js` 的 `REALMS` 表）。
pub struct RealmConfig {
    pub id: &'static str,
    pub label: &'static str,
    /// 对话 host（`llm_utils_chat` / `get_detail_param`）。
    pub chat_host: &'static str,
    /// 鉴权 host（`ExchangeToken` 刷新）。
    pub auth_host: &'static str,
    /// 默认 `x-ide-version`。
    pub ide_version: &'static str,
    /// 该区域对应的客户端数据目录名（`%APPDATA%` 下的子目录）。
    pub product_dirs: &'static [&'static str],
    /// OAuth ClientID。
    pub oauth_client_id: &'static str,
    /// 从 host/issuer 字符串里认区域的**子串**标记。
    pub match_hosts: &'static [&'static str],
    /// 从 `userRegion.region` 认区域的**区域码**。
    pub match_regions: &'static [&'static str],
}

const REALM_CN: RealmConfig = RealmConfig {
    id: "cn",
    label: "Trae 国内版 (China)",
    chat_host: "https://trae-api-cn.mchost.guru",
    auth_host: "https://api.trae.cn",
    ide_version: "3.3.67",
    product_dirs: &["TRAE SOLO CN", "Trae CN", "TRAE CN"],
    oauth_client_id: "en1oxy7wnw8j9n",
    match_hosts: &["trae.cn", "mchost.guru"],
    match_regions: &["CN"],
};

const REALM_SG: RealmConfig = RealmConfig {
    id: "sg",
    label: "Trae 国际版 (Global)",
    chat_host: "https://coresg-normal.trae.ai",
    auth_host: "https://growsg-normal.trae.ai",
    ide_version: "3.5.51",
    product_dirs: &["TRAE SOLO", "Trae", "TRAE"],
    oauth_client_id: "ono9krqynydwx5",
    match_hosts: &["growsg", "coresg", "coreva", "trae.ai"],
    match_regions: &["SG", "US"],
};

/// 美国节点的对话 host（**只有** `realm == Sg && userRegion == "US"` 才用）。
pub const US_CHAT_HOST: &str = "https://coreva-normal.trae.ai";

/// 兜底区域。
///
/// 刻意是 `Cn`：宿主侧全部既有 Trae 代码都硬编码 `api.trae.cn`，
/// 默认值一改就会**静默改变已上线行为**（把国服账号的刷新发去新加坡）。
pub const DEFAULT_REALM: Realm = Realm::Cn;

pub const REALM_IDS: [Realm; 2] = [Realm::Cn, Realm::Sg];

pub fn realm_config(realm: Realm) -> &'static RealmConfig {
    match realm {
        Realm::Cn => &REALM_CN,
        Realm::Sg => &REALM_SG,
    }
}

/// 区域是怎么判出来的（用于日志与「这是探测值还是兜底值」的区分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealmEvidence {
    /// 凭据里显式写了区域 —— **最权威**。
    Explicit,
    /// 从 host / issuer 字符串里认出来的。
    HostOrIssuer,
    /// 从 `userRegion.region` 区域码认出来的。
    UserRegion,
    /// 都没认出来，用了兜底区域。
    Default,
}

impl RealmEvidence {
    /// 是否是**探测**出来的（`false` = 兜底猜的）。
    ///
    /// 调用方拿它决定要不要打日志 —— 兜底值在实盘是常态（JWT 无 `iss`），
    /// 每次都打会淹没真信号。
    pub fn is_detected(self) -> bool {
        !matches!(self, RealmEvidence::Default)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RealmEvidence::Explicit => "explicit",
            RealmEvidence::HostOrIssuer => "hostOrIssuer",
            RealmEvidence::UserRegion => "userRegion",
            RealmEvidence::Default => "default",
        }
    }
}

/// 区域判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RealmGuess {
    pub realm: Realm,
    pub evidence: RealmEvidence,
}

/// 判定区域的输入线索（全部可选 —— 凭证里有什么就填什么）。
#[derive(Debug, Clone, Default)]
pub struct CredentialHint<'a> {
    /// 凭据里显式写的区域（`_edition` / `realm`）。
    pub explicit: Option<&'a str>,
    /// 凭据里的 host（`auth_host` / `chat_host`）。
    pub host: Option<&'a str>,
    /// JWT 的 `iss` / `issuer`。
    pub issuer: Option<&'a str>,
    /// `userRegion.region` 之类的区域码。
    pub region: Option<&'a str>,
}

/// 按「显式 → host/issuer → 区域码 → 兜底」判定区域。
pub fn realm_from_credential(hint: &CredentialHint<'_>) -> RealmGuess {
    if let Some(explicit) = hint.explicit {
        if let Some(realm) = Realm::parse(explicit) {
            return RealmGuess { realm, evidence: RealmEvidence::Explicit };
        }
    }

    let probe = format!("{} {}", hint.host.unwrap_or(""), hint.issuer.unwrap_or("")).to_ascii_lowercase();
    if !probe.trim().is_empty() {
        for realm in REALM_IDS {
            if realm_config(realm).match_hosts.iter().any(|m| probe.contains(m)) {
                return RealmGuess { realm, evidence: RealmEvidence::HostOrIssuer };
            }
        }
    }

    if let Some(region) = hint.region {
        let code = region.trim().to_ascii_uppercase();
        if !code.is_empty() {
            for realm in REALM_IDS {
                if realm_config(realm).match_regions.contains(&code.as_str()) {
                    return RealmGuess { realm, evidence: RealmEvidence::UserRegion };
                }
            }
        }
    }

    RealmGuess { realm: DEFAULT_REALM, evidence: RealmEvidence::Default }
}

/// 默认对话 host（不读环境变量）。
pub fn chat_host_default(realm: Realm, region_code: &str) -> &'static str {
    if realm == Realm::Sg && region_code.trim().eq_ignore_ascii_case("US") {
        return US_CHAT_HOST;
    }
    realm_config(realm).chat_host
}

/// 默认鉴权 host（不读环境变量）。
pub fn auth_host_default(realm: Realm) -> &'static str {
    realm_config(realm).auth_host
}

fn env_override(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

/// 对话 host（带环境变量覆盖 `TRAE_HOST_US` / `TRAE_HOST_CN` / `TRAE_HOST_SG`）。
///
/// ★ 美区是 **SG realm 的一个节点**（`coreva-normal`），不是独立的 realm：账号的
///   `userRegion` 为 US 时，SG realm 的令牌必须发去 `coreva`，发去 `coresg` 会
///   吃上游风控（4017）。故 US 分支**先于** `TRAE_HOST_SG` 生效 —— 否则一个
///   设了 `TRAE_HOST_SG` 的调试环境会把美区账号的路由悄悄改回新加坡。
pub fn chat_host(realm: Realm, region_code: &str) -> String {
    if realm == Realm::Sg && region_code.trim().eq_ignore_ascii_case("US") {
        return env_override("TRAE_HOST_US").unwrap_or_else(|| US_CHAT_HOST.to_string());
    }
    let key = match realm {
        Realm::Cn => "TRAE_HOST_CN",
        Realm::Sg => "TRAE_HOST_SG",
    };
    env_override(key).unwrap_or_else(|| realm_config(realm).chat_host.to_string())
}

/// 鉴权 host（带环境变量覆盖 `TRAE_AUTH_HOST_CN` / `TRAE_AUTH_HOST_SG`）。
pub fn auth_host(realm: Realm) -> String {
    let key = match realm {
        Realm::Cn => "TRAE_AUTH_HOST_CN",
        Realm::Sg => "TRAE_AUTH_HOST_SG",
    };
    env_override(key).unwrap_or_else(|| realm_config(realm).auth_host.to_string())
}

/// 默认 `x-ide-version`（带环境变量覆盖 `TRAE_IDE_VERSION_CN` / `_SG`）。
pub fn ide_version(realm: Realm) -> String {
    let key = match realm {
        Realm::Cn => "TRAE_IDE_VERSION_CN",
        Realm::Sg => "TRAE_IDE_VERSION_SG",
    };
    env_override(key).unwrap_or_else(|| realm_config(realm).ide_version.to_string())
}

/// OAuth ClientID（带环境变量覆盖 `TRAE_OAUTH_CLIENT_ID_CN` / `_SG`）。
///
/// ★ 与 `trae_refresh::OAUTH_CLIENT_ID`（写死 `ono9krqynydwx5` = 国际版值）
/// **不同**：刷新用的 ClientID 必须跟着**账号自己的产品线**走，
/// 拿国际版 ClientID 去刷国服账号会被拒。
pub fn oauth_client_id(realm: Realm) -> String {
    let key = match realm {
        Realm::Cn => "TRAE_OAUTH_CLIENT_ID_CN",
        Realm::Sg => "TRAE_OAUTH_CLIENT_ID_SG",
    };
    env_override(key).unwrap_or_else(|| realm_config(realm).oauth_client_id.to_string())
}

/// 只在该区域提供的模型前缀（`realms.js` 的 `REALM_EXCLUSIVE_MODELS`）。
///
/// 用途只有一个：**客户端没写区域前缀时**，按模型名反推该打哪个区域。
/// 命中多个区域（或一个都不命中）时返回 `None`，交给调用方用账号自己的区域。
const REALM_EXCLUSIVE_MODELS_SG: &[&str] = &[
    "gpt-5", "gpt-6", "gpt-4", "gemini", "kimi-k", "minimax", "deepseek-v3", "deepseek-chat",
    "deepseek-reasoner", "dola-seed", "doubao-for-auto",
];
const REALM_EXCLUSIVE_MODELS_CN: &[&str] = &[
    "glm", "qwen", "doubao-seed", "kimi-k3", "kimi-k2.6", "kimi-k2.7", "deepseek-v4",
    "seed-code", "sagitta", "aquila", "browser_use",
];

pub fn realm_exclusive_models(realm: Realm) -> &'static [&'static str] {
    match realm {
        Realm::Cn => REALM_EXCLUSIVE_MODELS_CN,
        Realm::Sg => REALM_EXCLUSIVE_MODELS_SG,
    }
}

/// 按模型名反推区域。
///
/// ★ 与 core 同款的两处细节，缺一个就会给出错误结论：
/// - **先剥渠道前缀**（取 `/ ` 之后那一段）：`Kimi-CN//kimi-k3` 该按
///   `kimi-k3` 判，按整串判会命中 `kimi-k`（国际版前缀）而判反；
/// - **恰好命中一个区域才返回**：`kimi-k3` 同时含 `kimi-k`（SG）与 `kimi-k3`（CN），
///   歧义时返回 `None` 比猜一个更安全。
pub fn infer_realm_for_model(model_name: &str, explicit_realm: Option<&str>) -> Option<Realm> {
    if let Some(explicit) = explicit_realm {
        if let Some(realm) = Realm::parse(explicit) {
            return Some(realm);
        }
    }
    let name = model_name.rsplit('/').next().unwrap_or(model_name).trim().to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    let mut hit: Option<Realm> = None;
    for realm in REALM_IDS {
        let matched = realm_exclusive_models(realm).iter().any(|p| name.starts_with(p) || name.contains(p));
        if matched {
            if hit.is_some() && hit != Some(realm) {
                return None; // 歧义
            }
            hit = Some(realm);
        }
    }
    hit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_the_documented_aliases() {
        for v in ["cn", "CN", " cn ", "solo", "solo-cn", "solo_cn", "SOLO"] {
            assert_eq!(Realm::parse(v), Some(Realm::Cn), "input={v:?}");
        }
        assert_eq!(Realm::parse("sg"), Some(Realm::Sg));
        assert_eq!(Realm::parse("SG"), Some(Realm::Sg));
        assert_eq!(Realm::parse("us"), None);
        assert_eq!(Realm::parse(""), None);
    }

    #[test]
    fn explicit_beats_everything_else() {
        let hint = CredentialHint {
            explicit: Some("sg"),
            host: Some("https://api.trae.cn"),
            issuer: None,
            region: Some("CN"),
        };
        let g = realm_from_credential(&hint);
        assert_eq!(g.realm, Realm::Sg);
        assert_eq!(g.evidence, RealmEvidence::Explicit);
        assert!(g.evidence.is_detected());
    }

    #[test]
    fn host_and_issuer_are_probed_together() {
        let g = realm_from_credential(&CredentialHint {
            host: Some("https://trae-api-cn.mchost.guru"),
            ..Default::default()
        });
        assert_eq!((g.realm, g.evidence), (Realm::Cn, RealmEvidence::HostOrIssuer));

        let g = realm_from_credential(&CredentialHint {
            issuer: Some("https://growsg-normal.trae.ai"),
            ..Default::default()
        });
        assert_eq!((g.realm, g.evidence), (Realm::Sg, RealmEvidence::HostOrIssuer));
    }

    #[test]
    fn user_region_is_uppercased_before_matching() {
        let g = realm_from_credential(&CredentialHint { region: Some("us"), ..Default::default() });
        assert_eq!((g.realm, g.evidence), (Realm::Sg, RealmEvidence::UserRegion));
    }

    /// 实盘形态：trae_accounts.json 里的 JWT **没有 iss**、也没有 _edition，
    /// 于是判定必然落到兜底 —— 此时必须是 Cn 且标记成「猜的」。
    #[test]
    fn falls_back_to_cn_and_says_so() {
        let g = realm_from_credential(&CredentialHint::default());
        assert_eq!(g.realm, Realm::Cn);
        assert_eq!(g.evidence, RealmEvidence::Default);
        assert!(!g.evidence.is_detected());
    }

    #[test]
    fn us_region_only_switches_the_chat_host_for_sg() {
        assert_eq!(chat_host_default(Realm::Sg, "US"), US_CHAT_HOST);
        assert_eq!(chat_host_default(Realm::Sg, "us"), US_CHAT_HOST);
        assert_eq!(chat_host_default(Realm::Sg, "SG"), "https://coresg-normal.trae.ai");
        // 国内版没有美国节点 —— 传 US 也不能改 host。
        assert_eq!(chat_host_default(Realm::Cn, "US"), "https://trae-api-cn.mchost.guru");
    }

    #[test]
    fn infer_realm_strips_the_channel_prefix_first() {
        // ★ 渠道前缀必须先剥掉，否则整串既含 "Kimi-CN"（不命中）又含 "kimi-k3"，
        //   而 kimi-k3 本身歧义（见下一个测试）⇒ 结论仍是「交回调用方」。
        assert_eq!(infer_realm_for_model("Kimi-CN//kimi-k3", None), None);
        // 剥前缀这一步是**可观测**的：同一个名字加前缀后结论不变（不会因为前缀而翻转）。
        assert_eq!(
            infer_realm_for_model("Any-Channel/gpt-5", None),
            infer_realm_for_model("gpt-5", None)
        );
        assert_eq!(infer_realm_for_model("Any-Channel/gpt-5", None), Some(Realm::Sg));
        assert_eq!(infer_realm_for_model("gpt-5.4", None), Some(Realm::Sg));
        assert_eq!(infer_realm_for_model("glm-5.3", None), Some(Realm::Cn));
    }

    #[test]
    fn infer_realm_returns_none_on_ambiguity_or_unknown() {
        // 同时含 kimi-k(SG) 与 kimi-k3(CN) ⇒ 歧义 ⇒ None，不猜。
        assert_eq!(infer_realm_for_model("kimi-k3", None), None);
        assert_eq!(infer_realm_for_model("no-such-model-zzz", None), None);
        assert_eq!(infer_realm_for_model("", None), None);
    }

    #[test]
    fn infer_realm_honours_an_explicit_value() {
        assert_eq!(infer_realm_for_model("glm-5.3", Some("sg")), Some(Realm::Sg));
    }

    #[test]
    fn table_matches_the_core_copy() {
        // 这几条是**跨 crate 契约**：core 的 trae_realm.rs 必须给出同样的值。
        assert_eq!(realm_config(Realm::Cn).chat_host, "https://trae-api-cn.mchost.guru");
        assert_eq!(realm_config(Realm::Cn).auth_host, "https://api.trae.cn");
        assert_eq!(realm_config(Realm::Sg).chat_host, "https://coresg-normal.trae.ai");
        assert_eq!(realm_config(Realm::Sg).auth_host, "https://growsg-normal.trae.ai");
        assert_eq!(realm_config(Realm::Cn).oauth_client_id, "en1oxy7wnw8j9n");
        assert_eq!(realm_config(Realm::Sg).oauth_client_id, "ono9krqynydwx5");
    }
}
