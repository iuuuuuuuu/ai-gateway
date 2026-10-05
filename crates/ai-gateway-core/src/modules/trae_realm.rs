//! Trae 双区域（国内 / 国际）区域表与选区判定。
//!
//! 来源：`trae-wangqi233/src/realms.js`（167 行）逐条移植。
//!
//! ## 为什么需要一张表
//!
//! Trae 跑着**两套互相独立的部署**（下称「区域」）：中国大陆服务与国际
//! （新加坡）服务。它们的差异远不止 base URL —— chat host、auth host、
//! 默认 IDE 版本、模型目录、乃至请求头里声明的产品身份全都不同。把这些事实
//! 收在一张表里（而不是散落成一堆 `if edition == "cn"`），每个差异才是可审计的。
//!
//! 这很重要，因为**把 token 发错区域会以极其困惑的方式失败**：不是明确的
//! 「区域不匹配」，而是 4001 `param is invalid`、404 TLB 页面，或者认证接口
//! 直接拒绝 —— 排查成本极高。
//!
//! ## ★ 硬约束：区域判定必须以**凭据自身**为准
//!
//! token 的 issuer 声明、以及客户端当时用的 auth host，是**账号的属性**；
//! 而「当前选中哪个区域」只是一个**路由选择**。因此判定顺序必须是：
//!
//! 1. 凭据里显式写明的区域（`_edition` / `realm`）
//! 2. host / issuer 里的区域标记
//! 3. `userRegion` 区域码
//! 4. 兜底默认值（[`DEFAULT_REALM`]）
//!
//! 反过来做（先按「当前选中的区域」给账号贴标签）会把**每一个国际账号静默
//! 误标成国内账号**，随后它的 token 刷新被发去中国 auth host —— 那必然失败。
//! [`realm_from_credential`] 因此额外返回 [`RealmEvidence`]，让调用方能区分
//! 「探测出来的」与「兜底猜的」，从而对后者给出警告而不是假装知道。
//!
//! ## 与现有 CN 路径的关系
//!
//! 本模块**只提供区域表与判定逻辑**，不修改任何既有模块的 `api.trae.cn`
//! 硬编码 —— 那条路径是已验证可用的。新代码若需要按区域取 host，请用
//! [`auth_host`] / [`chat_host`]，其默认区域就是 CN，因此不改变现有行为。

/// 区域标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Realm {
    /// 中国大陆版。
    Cn,
    /// 国际版（新加坡）。
    Sg,
}

impl Realm {
    /// 表里的 id 字符串（`"cn"` / `"sg"`）。
    pub fn as_str(self) -> &'static str {
        match self {
            Realm::Cn => "cn",
            Realm::Sg => "sg",
        }
    }

    /// 人类可读标签。
    pub fn label(self) -> &'static str {
        match self {
            Realm::Cn => "Trae 国内版 (China)",
            Realm::Sg => "Trae 国际版 (Global)",
        }
    }

    /// 解析区域 id（大小写不敏感，接受 `solo` 系别名）。
    ///
    /// 与 JS 的 `normalizeRealm` 一致：`solo` / `solo-cn` / `solo_cn` 都归一为 CN。
    pub fn parse(value: &str) -> Option<Realm> {
        let v = value.trim().to_ascii_lowercase();
        match v.as_str() {
            "solo" | "solo-cn" | "solo_cn" => Some(Realm::Cn),
            "cn" => Some(Realm::Cn),
            "sg" => Some(Realm::Sg),
            _ => None,
        }
    }
}

/// 单个区域的完整配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RealmConfig {
    pub id: &'static str,
    pub label: &'static str,
    /// Chat / LLM 端点。**与 auth host 刻意分开**：混用会拿到 404 TLB 页面。
    pub chat_host: &'static str,
    /// ExchangeToken / token 刷新所在端点 —— **不是** chat host。
    pub auth_host: &'static str,
    /// 读不到本地客户端清单时的兜底 IDE 版本。
    pub ide_version: &'static str,
    /// 可能存放该区域客户端登录态的数据目录名（`%APPDATA%` 子目录）。
    pub product_dirs: &'static [&'static str],
    /// 用 refresh token 换 token 时使用的 OAuth client id。
    pub oauth_client_id: &'static str,
    /// 出现在凭据 / token 声明里的区域标记（子串匹配，大小写不敏感）。
    pub match_hosts: &'static [&'static str],
    /// 出现在 `userRegion` 里的区域码。
    pub match_regions: &'static [&'static str],
}

/// 国内版配置（对应 `realms.js` 的 `REALMS.cn`，L20-37）。
pub const REALM_CN: RealmConfig = RealmConfig {
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

/// 国际版配置（对应 `realms.js` 的 `REALMS.sg`，L38-48）。
pub const REALM_SG: RealmConfig = RealmConfig {
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

/// 美国部署与「国际版」是同一产品，但**有独立的 chat host**。
///
/// 只有 chat 路由不同；认证仍然走国际版的 auth host。
pub const US_CHAT_HOST: &str = "https://coreva-normal.trae.ai";

/// 兜底区域。
///
/// ★ 与既有 CN 路径保持一致 —— 现有模块全部硬编码 `api.trae.cn`，
/// 默认值若改成别的区域会**静默改变已上线行为**。
pub const DEFAULT_REALM: Realm = Realm::Cn;

/// 全部区域（顺序即判定优先级，与 JS `REALM_IDS` 一致：cn 先于 sg）。
pub const REALM_IDS: [Realm; 2] = [Realm::Cn, Realm::Sg];

/// 取区域配置。
pub fn realm_config(realm: Realm) -> &'static RealmConfig {
    match realm {
        Realm::Cn => &REALM_CN,
        Realm::Sg => &REALM_SG,
    }
}

/// 区域判定所用的证据来源。
///
/// 调用方**应当**区分 [`RealmEvidence::Default`]：那意味着什么都没探测到，
/// 区域只是猜的（见模块头「硬约束」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealmEvidence {
    /// 凭据里显式写明。
    Explicit,
    /// 由 host / issuer 的区域标记判定 —— **权威方向**。
    HostOrIssuer,
    /// 由 `userRegion` 区域码判定。
    UserRegion,
    /// 什么都没匹配上，用了 [`DEFAULT_REALM`]。
    Default,
}

impl RealmEvidence {
    /// 该区域是否是**探测出来的**（而非兜底猜的）。
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

/// 区域判定结果：区域 + 判定依据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RealmGuess {
    pub realm: Realm,
    pub evidence: RealmEvidence,
}

/// 凭据里的区域信息（由调用方从 storage.json / JWT 中提取后传入）。
///
/// 之所以用入参而不是在这里直接读文件 / 解 JWT：判定必须是**纯函数**才可测，
/// 且本模块不应依赖 `trae_discover` 或 `trae_account` 的具体形态。
#[derive(Debug, Clone, Default)]
pub struct CredentialHint<'a> {
    /// 显式区域（`_edition` 或 `realm` 字段）。
    pub explicit: Option<&'a str>,
    /// 客户端使用的 auth / chat host。
    pub host: Option<&'a str>,
    /// token 的 `iss` / `issuer` 声明。
    pub issuer: Option<&'a str>,
    /// `userRegion.region` 区域码（如 `CN` / `SG` / `US`）。
    pub region: Option<&'a str>,
}

/// ★ 由**凭据自身**判定区域 —— 这是权威方向。
///
/// 判定顺序见模块头。返回 [`RealmGuess`] 而非裸 [`Realm`]，
/// 是为了让调用方能把「兜底猜的」与「探测到的」区别对待。
pub fn realm_from_credential(hint: &CredentialHint<'_>) -> RealmGuess {
    // 1) 显式区域优先
    if let Some(explicit) = hint.explicit {
        if let Some(realm) = Realm::parse(explicit) {
            return RealmGuess {
                realm,
                evidence: RealmEvidence::Explicit,
            };
        }
    }

    // 2) host 与 issuer 的区域标记（两者拼在一起探测，与 JS 的 probe 一致）
    let probe = format!(
        "{} {}",
        hint.host.unwrap_or(""),
        hint.issuer.unwrap_or("")
    )
    .to_ascii_lowercase();
    if !probe.trim().is_empty() {
        for realm in REALM_IDS {
            let cfg = realm_config(realm);
            if cfg
                .match_hosts
                .iter()
                .any(|m| probe.contains(&m.to_ascii_lowercase()))
            {
                return RealmGuess {
                    realm,
                    evidence: RealmEvidence::HostOrIssuer,
                };
            }
        }
    }

    // 3) userRegion 区域码
    if let Some(region) = hint.region {
        let code = region.trim().to_ascii_uppercase();
        if !code.is_empty() {
            for realm in REALM_IDS {
                let cfg = realm_config(realm);
                if cfg.match_regions.iter().any(|r| *r == code) {
                    return RealmGuess {
                        realm,
                        evidence: RealmEvidence::UserRegion,
                    };
                }
            }
        }
    }

    // 4) 兜底
    RealmGuess {
        realm: DEFAULT_REALM,
        evidence: RealmEvidence::Default,
    }
}

/// 区域 + 区域码 → chat host（**不含**环境变量覆盖）。
///
/// 美国部署在国际版下走独立 chat host（[`US_CHAT_HOST`]），但认证仍走 SG。
pub fn chat_host_default(realm: Realm, region_code: &str) -> &'static str {
    let code = region_code.trim().to_ascii_uppercase();
    if realm == Realm::Sg && code == "US" {
        return US_CHAT_HOST;
    }
    realm_config(realm).chat_host
}

/// 区域 → auth host（**不含**环境变量覆盖）。
pub fn auth_host_default(realm: Realm) -> &'static str {
    realm_config(realm).auth_host
}

/// 读一个非空的环境变量覆盖值。
fn env_override(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// 区域 + 区域码 → chat host，支持环境变量覆盖。
///
/// 环境变量名与 JS 参考实现一致：`TRAE_HOST_CN` / `TRAE_HOST_SG` / `TRAE_HOST_US`。
pub fn chat_host(realm: Realm, region_code: &str) -> String {
    let key = if realm == Realm::Sg && region_code.trim().eq_ignore_ascii_case("US") {
        "TRAE_HOST_US"
    } else {
        match realm {
            Realm::Cn => "TRAE_HOST_CN",
            Realm::Sg => "TRAE_HOST_SG",
        }
    };
    env_override(key).unwrap_or_else(|| chat_host_default(realm, region_code).to_string())
}

/// 区域 → auth host，支持环境变量覆盖（`TRAE_AUTH_HOST_CN` / `TRAE_AUTH_HOST_SG`）。
pub fn auth_host(realm: Realm) -> String {
    let key = match realm {
        Realm::Cn => "TRAE_AUTH_HOST_CN",
        Realm::Sg => "TRAE_AUTH_HOST_SG",
    };
    env_override(key).unwrap_or_else(|| auth_host_default(realm).to_string())
}

/// 区域 → 默认 IDE 版本，支持环境变量覆盖。
pub fn ide_version(realm: Realm) -> String {
    let key = match realm {
        Realm::Cn => "TRAE_IDE_VERSION_CN",
        Realm::Sg => "TRAE_IDE_VERSION_SG",
    };
    env_override(key).unwrap_or_else(|| realm_config(realm).ide_version.to_string())
}

/// 区域 → OAuth client id，支持环境变量覆盖。
pub fn oauth_client_id(realm: Realm) -> String {
    let key = match realm {
        Realm::Cn => "TRAE_OAUTH_CLIENT_ID_CN",
        Realm::Sg => "TRAE_OAUTH_CLIENT_ID_SG",
    };
    env_override(key).unwrap_or_else(|| realm_config(realm).oauth_client_id.to_string())
}

/// **只在一个区域的目录里存在**的模型名（小写、前缀匹配）。
///
/// 两套部署的模型目录几乎不相交（2026-09-17 实测：47 个名字，仅 3 个共有，
/// 其中一个还是内部辅助模型）。错区域的请求会被答以 4001 `param is invalid`，
/// **完全不会提到区域**，所以这份清单是「按模型名路由」的唯一依据。
///
/// 保持小写；匹配大小写不敏感且按前缀，因此家族变体
/// （`gpt-5.6-terra`、`gemini-3-flash-auto` …）无需逐个列出。
pub const REALM_EXCLUSIVE_MODELS_SG: &[&str] = &[
    "gpt-5",
    "gpt-6",
    "gpt-4",
    "gemini",
    "kimi-k",
    "minimax",
    "deepseek-v3",
    "deepseek-chat",
    "deepseek-reasoner",
    "dola-seed",
    "doubao-for-auto",
];

/// 国内版独占模型名（见 [`REALM_EXCLUSIVE_MODELS_SG`] 的说明）。
pub const REALM_EXCLUSIVE_MODELS_CN: &[&str] = &[
    "glm",
    "qwen",
    "doubao-seed",
    "kimi-k3",
    "kimi-k2.6",
    "kimi-k2.7",
    "deepseek-v4",
    "seed-code",
    "sagitta",
    "aquila",
    "browser_use",
];

/// 取某区域的独占模型名清单。
pub fn realm_exclusive_models(realm: Realm) -> &'static [&'static str] {
    match realm {
        Realm::Cn => REALM_EXCLUSIVE_MODELS_CN,
        Realm::Sg => REALM_EXCLUSIVE_MODELS_SG,
    }
}

/// ★ **兜底**用：按模型名推断该由哪个区域服务。
///
/// 模型配置里显式的 `realm` 字段永远优先；本函数只服务于那些早于区域路由
/// 存在的配置。**返回 `None` 而不是猜一个** —— 名字有歧义或完全未知时，
/// 把决定权交回调用方（保留其固定 / 全局区域），比猜错区域更安全。
///
/// ★ 已知局限：家族前缀会让 `kimi-k*` 天然歧义 —— `kimi-k3`（国内）
/// 同时命中 SG 的 `kimi-k` 前缀与 CN 的 `kimi-k3`，因此命中两个区域 ⇒ 返回
/// `None`。这是**刻意**的：宁可交回调用方，也不把国内模型路由去国际版。
pub fn infer_realm_for_model(model_name: &str, explicit_realm: Option<&str>) -> Option<Realm> {
    if let Some(explicit) = explicit_realm {
        if let Some(realm) = Realm::parse(explicit) {
            return Some(realm);
        }
    }
    let name = model_name.trim().to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    // 剥掉 `渠道/` 前缀，只看最后一段
    let roots = name.rsplit('/').next().unwrap_or(name.as_str());

    let hits: Vec<Realm> = REALM_IDS
        .iter()
        .copied()
        .filter(|realm| {
            realm_exclusive_models(*realm)
                .iter()
                .any(|p| roots.starts_with(p) || roots.contains(p))
        })
        .collect();

    // 恰好命中一个区域才给结论；歧义/未知一律交回调用方
    if hits.len() == 1 {
        Some(hits[0])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 区域表与_js_参考实现逐字段一致() {
        // 这些字面量全部来自 realms.js L20-48，改错任何一个都会让区域路由失效
        assert_eq!(REALM_CN.id, "cn");
        assert_eq!(REALM_CN.chat_host, "https://trae-api-cn.mchost.guru");
        assert_eq!(REALM_CN.auth_host, "https://api.trae.cn");
        assert_eq!(REALM_CN.ide_version, "3.3.67");
        assert_eq!(REALM_CN.product_dirs, &["TRAE SOLO CN", "Trae CN", "TRAE CN"]);
        assert_eq!(REALM_CN.oauth_client_id, "en1oxy7wnw8j9n");

        assert_eq!(REALM_SG.id, "sg");
        assert_eq!(REALM_SG.chat_host, "https://coresg-normal.trae.ai");
        assert_eq!(REALM_SG.auth_host, "https://growsg-normal.trae.ai");
        assert_eq!(REALM_SG.ide_version, "3.5.51");
        assert_eq!(REALM_SG.product_dirs, &["TRAE SOLO", "Trae", "TRAE"]);
        assert_eq!(REALM_SG.oauth_client_id, "ono9krqynydwx5");

        assert_eq!(US_CHAT_HOST, "https://coreva-normal.trae.ai");
        assert_eq!(realm_config(Realm::Cn).id, "cn");
        assert_eq!(realm_config(Realm::Sg).id, "sg");
        assert_eq!(Realm::Cn.as_str(), "cn");
        assert_eq!(Realm::Sg.as_str(), "sg");
    }

    /// ★ 回归：默认区域必须是 CN，否则会**静默改变**既有硬编码 `api.trae.cn`
    /// 的 9 个模块的行为。
    #[test]
    fn 默认区域仍是国内且_auth_host_不变() {
        assert_eq!(DEFAULT_REALM, Realm::Cn);
        assert_eq!(auth_host_default(DEFAULT_REALM), "https://api.trae.cn");
        // 与既有模块硬编码的字符串逐字相同
        assert_eq!(
            auth_host_default(Realm::Cn),
            "https://api.trae.cn",
            "既有模块硬编码的就是这个值，改动会静默改变已上线行为"
        );
    }

    #[test]
    fn 显式区域优先于一切() {
        let hint = CredentialHint {
            explicit: Some("sg"),
            // 其余线索全部指向国内，显式字段仍应胜出
            host: Some("https://api.trae.cn"),
            issuer: Some("api.trae.cn"),
            region: Some("CN"),
        };
        let g = realm_from_credential(&hint);
        assert_eq!(g.realm, Realm::Sg);
        assert_eq!(g.evidence, RealmEvidence::Explicit);

        // `_edition` 的别名形态
        for alias in ["solo", "solo-cn", "solo_cn", "SOLO"] {
            let hint = CredentialHint {
                explicit: Some(alias),
                ..Default::default()
            };
            assert_eq!(
                realm_from_credential(&hint).realm,
                Realm::Cn,
                "别名 {alias} 应归一为国内"
            );
        }
        // 无法识别的显式值不应被当成有效判定，必须继续往下探测
        let hint = CredentialHint {
            explicit: Some("mars"),
            host: Some("https://growsg-normal.trae.ai"),
            ..Default::default()
        };
        let g = realm_from_credential(&hint);
        assert_eq!(g.realm, Realm::Sg);
        assert_eq!(g.evidence, RealmEvidence::HostOrIssuer);
    }

    /// ★★ 本模块的核心硬约束：issuer / auth host 是**账号属性**，
    /// 优先于 userRegion。把国际账号按 userRegion 误判成国内，
    /// 会让它的 token 刷新被发去中国 auth host 而必然失败。
    #[test]
    fn issuer_与_host_优先于_userRegion() {
        // 国际账号：issuer 是国际 auth host，但 userRegion 恰好是 CN
        // （例如用户在中国用国际版）—— 必须以 issuer 为准
        let hint = CredentialHint {
            issuer: Some("https://growsg-normal.trae.ai"),
            region: Some("CN"),
            ..Default::default()
        };
        let g = realm_from_credential(&hint);
        assert_eq!(
            g.realm,
            Realm::Sg,
            "issuer 指向国际版时不得被 userRegion=CN 拉回国内"
        );
        assert_eq!(g.evidence, RealmEvidence::HostOrIssuer);

        // 反向：国内 issuer + SG 区域码
        let hint = CredentialHint {
            host: Some("https://api.trae.cn"),
            region: Some("SG"),
            ..Default::default()
        };
        let g = realm_from_credential(&hint);
        assert_eq!(g.realm, Realm::Cn);
        assert_eq!(g.evidence, RealmEvidence::HostOrIssuer);

        // 只有区域码可用时才用区域码
        let hint = CredentialHint {
            region: Some("sg"),
            ..Default::default()
        };
        let g = realm_from_credential(&hint);
        assert_eq!(g.realm, Realm::Sg);
        assert_eq!(g.evidence, RealmEvidence::UserRegion);

        // US 区域码属于国际版
        let hint = CredentialHint {
            region: Some("US"),
            ..Default::default()
        };
        assert_eq!(realm_from_credential(&hint).realm, Realm::Sg);
    }

    /// ★ 探测不到时必须**如实标注**为兜底，让调用方能给出警告 ——
    /// 而不是让调用方以为「已经知道了」。
    #[test]
    fn 无任何线索时回退默认且证据标为兜底() {
        for hint in [
            CredentialHint::default(),
            CredentialHint {
                host: Some(""),
                issuer: Some("   "),
                region: Some(""),
                ..Default::default()
            },
            CredentialHint {
                // 认不出的 host / 区域码
                host: Some("https://example.com"),
                region: Some("MARS"),
                ..Default::default()
            },
        ] {
            let g = realm_from_credential(&hint);
            assert_eq!(g.realm, DEFAULT_REALM);
            assert_eq!(g.evidence, RealmEvidence::Default);
            assert!(
                !g.evidence.is_detected(),
                "兜底猜的区域必须能被调用方识别出来"
            );
        }
        assert!(RealmEvidence::Explicit.is_detected());
        assert!(RealmEvidence::HostOrIssuer.is_detected());
        assert!(RealmEvidence::UserRegion.is_detected());
        assert_eq!(RealmEvidence::Default.as_str(), "default");
    }

    #[test]
    fn 区域标记匹配大小写不敏感且不误伤() {
        // 大小写混写
        for host in [
            "HTTPS://API.TRAE.CN",
            "https://api.trae.cn",
            "https://Api.Trae.Cn/x",
        ] {
            let hint = CredentialHint {
                host: Some(host),
                ..Default::default()
            };
            assert_eq!(
                realm_from_credential(&hint).realm,
                Realm::Cn,
                "{host} 应判为国内"
            );
        }
        for host in [
            "https://CORESG-normal.trae.ai",
            "https://growsg-normal.trae.ai",
            "https://coreva-normal.trae.ai",
        ] {
            let hint = CredentialHint {
                host: Some(host),
                ..Default::default()
            };
            assert_eq!(
                realm_from_credential(&hint).realm,
                Realm::Sg,
                "{host} 应判为国际"
            );
        }
        // `nottrae.cn` 不该被误判（子串 `trae.cn` 确实在其中，
        // 这是 JS 正则 `/trae\.cn/i` 的既有行为 —— 如实记录，不擅自收紧）
        let hint = CredentialHint {
            host: Some("https://nottrae.cn"),
            ..Default::default()
        };
        assert_eq!(
            realm_from_credential(&hint).realm,
            Realm::Cn,
            "与 JS 正则行为一致：子串命中即算命中"
        );
    }

    #[test]
    fn 美国区域在国际版下走独立_chat_host_但认证仍走_sg() {
        assert_eq!(chat_host_default(Realm::Sg, "US"), US_CHAT_HOST);
        assert_eq!(chat_host_default(Realm::Sg, "us"), US_CHAT_HOST);
        assert_eq!(chat_host_default(Realm::Sg, "SG"), REALM_SG.chat_host);
        // 认证**不**因 US 而改变
        assert_eq!(auth_host_default(Realm::Sg), "https://growsg-normal.trae.ai");
        // US 只对国际版生效，国内版不受影响
        assert_eq!(chat_host_default(Realm::Cn, "US"), REALM_CN.chat_host);
    }

    /// ★ 模型名推断是**兜底**：只在恰好命中一个区域时给结论。
    #[test]
    fn 模型名推断只在唯一命中时给出区域() {
        // 国际版独占
        for m in ["gpt-5", "gpt-5.6-terra", "gemini-3-flash-auto", "deepseek-v3"] {
            assert_eq!(
                infer_realm_for_model(m, None),
                Some(Realm::Sg),
                "{m} 应判为国际版独占"
            );
        }
        // 国内版独占
        for m in ["glm-4.6", "qwen3", "doubao-seed-1.6", "deepseek-v4", "seed-code"] {
            assert_eq!(
                infer_realm_for_model(m, None),
                Some(Realm::Cn),
                "{m} 应判为国内版独占"
            );
        }
        // 完全未知 / 空 → 交回调用方
        assert_eq!(infer_realm_for_model("totally-unknown-model", None), None);
        assert_eq!(infer_realm_for_model("", None), None);
        assert_eq!(infer_realm_for_model("   ", None), None);
    }

    /// ★ 已知局限的**显式**记录：`kimi-k` 家族前缀天然歧义。
    ///
    /// `kimi-k3`（国内独占）同时命中国际版的 `kimi-k` 前缀与国内的 `kimi-k3`，
    /// 于是命中两个区域 ⇒ 返回 `None`。这是刻意选择：宁可交回调用方，
    /// 也不把国内模型路由去国际版（错区域的 4001 完全不会提到区域，极难排查）。
    #[test]
    fn 模型名歧义的_kimi_k_族必须交回调用方() {
        for m in ["kimi-k3", "kimi-k2.6", "kimi-k2.7"] {
            assert_eq!(
                infer_realm_for_model(m, None),
                None,
                "{m} 同时命中两个区域的前缀，必须交回调用方而不是猜"
            );
        }
        // 显式指定时仍然能定下来
        assert_eq!(
            infer_realm_for_model("kimi-k3", Some("cn")),
            Some(Realm::Cn)
        );
    }

    #[test]
    fn 渠道前缀被剥离且显式区域优先() {
        // `渠道/` 前缀不得干扰匹配
        assert_eq!(
            infer_realm_for_model("my-channel/gpt-5", None),
            Some(Realm::Sg)
        );
        assert_eq!(
            infer_realm_for_model("my-channel/glm-4", None),
            Some(Realm::Cn)
        );
        // 显式区域压过模型名推断（哪怕模型名指向另一个区域）
        assert_eq!(
            infer_realm_for_model("gpt-5", Some("cn")),
            Some(Realm::Cn),
            "显式 realm 永远优先于模型名推断"
        );
        assert_eq!(
            infer_realm_for_model("glm-4", Some("sg")),
            Some(Realm::Sg)
        );
        // 无法识别的显式值 → 退回模型名推断
        assert_eq!(
            infer_realm_for_model("gpt-5", Some("mars")),
            Some(Realm::Sg)
        );
        // 大小写不敏感
        assert_eq!(infer_realm_for_model("GPT-5", None), Some(Realm::Sg));
    }

    /// 两组独占清单不得重合 —— 重合会让该模型名永远歧义。
    #[test]
    fn 两组独占模型清单无逐字重合() {
        for a in REALM_EXCLUSIVE_MODELS_CN {
            assert!(
                !REALM_EXCLUSIVE_MODELS_SG.contains(a),
                "{a} 同时出现在两组清单里，会导致该名字永远歧义"
            );
        }
        assert!(!realm_exclusive_models(Realm::Cn).is_empty());
        assert!(!realm_exclusive_models(Realm::Sg).is_empty());
    }

    /// 区域目录名不得重合 —— 重合会让「按目录判断区域」失效。
    #[test]
    fn 两区域产品目录不重合() {
        for d in REALM_CN.product_dirs {
            assert!(
                !REALM_SG.product_dirs.contains(d),
                "目录 {d} 同时属于两个区域"
            );
        }
        // 既有 discover 逻辑认的目录名必须仍在表内
        assert!(REALM_CN.product_dirs.contains(&"Trae CN"));
        assert!(REALM_CN.product_dirs.contains(&"TRAE SOLO CN"));
        assert!(REALM_SG.product_dirs.contains(&"TRAE SOLO"));
    }

    #[test]
    fn 区域解析接受别名并拒绝未知值() {
        assert_eq!(Realm::parse("cn"), Some(Realm::Cn));
        assert_eq!(Realm::parse("CN"), Some(Realm::Cn));
        assert_eq!(Realm::parse(" sg "), Some(Realm::Sg));
        assert_eq!(Realm::parse("SG"), Some(Realm::Sg));
        assert_eq!(Realm::parse("solo"), Some(Realm::Cn));
        assert_eq!(Realm::parse("solo-cn"), Some(Realm::Cn));
        assert_eq!(Realm::parse("solo_cn"), Some(Realm::Cn));
        assert_eq!(Realm::parse("Solo_CN"), Some(Realm::Cn));
        assert_eq!(Realm::parse("mars"), None);
        assert_eq!(Realm::parse(""), None);
        assert_eq!(Realm::parse("   "), None);
        // 两区域互不相等，标签可读
        assert_ne!(Realm::Cn, Realm::Sg);
        assert!(Realm::Cn.label().contains("国内"));
        assert!(Realm::Sg.label().contains("国际"));
    }
}
