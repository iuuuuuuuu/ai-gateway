//! ZCode 服务商（Z.AI / 智谱）与端点表。
//!
//! 移植自 Go `wt-port\go-gateway\internal\zcode\provider.go`（235 行），逐字对齐。
//!
//! # 为什么是「服务商」而不是「区域」
//!
//! Qoder 的国服/国际版是**同一产品的两个区域**（同一套协议、不同域名）；
//! ZCode 的 Z.AI 与智谱是**两个不同服务商**：域名不同（`api.z.ai` vs
//! `open.bigmodel.cn`）、凭证来源不同、凭证形态略有差异（Z.AI 的 secret 必需、
//! 智谱可选），但**协议相同**。
//!
//! 参考实现里智谱 host 有两处不一致写法（`bigmodel.cn` 与 `open.bigmodel.cn`）
//! —— 实测**两者都可达且返回相同**，本包统一用 `open.bigmodel.cn`。

/// ZCode 服务商。
///
/// ★ `Unknown` **不猜**：凭证里可能没有 provider 字段（用户手写扁平形），
/// 默认成某一个会让另一服务商的账号打到错误端点必然失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Provider {
    /// Z.AI（`api.z.ai`）。
    Zai,
    /// 智谱（`open.bigmodel.cn`）。
    Bigmodel,
    /// 未指定 —— 凭证里没写 provider。
    #[default]
    Unknown,
}

impl Provider {
    /// 落盘/配置里的字符串形态。`Unknown` 是空串（与 Go 的 `ProviderUnknown = ""` 一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Zai => "zai",
            Provider::Bigmodel => "bigmodel",
            Provider::Unknown => "",
        }
    }

    /// 界面上显示的名字。
    pub fn label(self) -> &'static str {
        match self {
            Provider::Zai => "Z.AI",
            Provider::Bigmodel => "智谱",
            Provider::Unknown => "未指定",
        }
    }

    /// 该服务商的四个 host。
    ///
    /// ★ `Unknown` 回落到 Z.AI 是**有意**的（未知多半是用户手写的凭证，
    /// Z.AI 更常见），但调用方应在界面上让用户确认。
    pub fn endpoints(self) -> Endpoints {
        match self {
            Provider::Bigmodel => Endpoints {
                openai_base: "https://open.bigmodel.cn/api/coding/paas/v4".to_string(),
                anthropic_base: "https://open.bigmodel.cn/api/anthropic".to_string(),
                biz_host: "https://open.bigmodel.cn".to_string(),
                quota_host: QUOTA_HOST.to_string(),
            },
            // Zai 与 Unknown 同一套端点。
            _ => Endpoints {
                openai_base: "https://api.z.ai/api/coding/paas/v4".to_string(),
                anthropic_base: "https://api.z.ai/api/anthropic".to_string(),
                biz_host: "https://api.z.ai".to_string(),
                quota_host: QUOTA_HOST.to_string(),
            },
        }
    }
}

/// 一个服务商的四个 host。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// API Key 通道（coding/paas）的 OpenAI 兼容端点。
    pub openai_base: String,
    /// API Key 通道的 Anthropic 兼容端点。
    pub anthropic_base: String,
    /// 该服务商自己的业务 host。
    pub biz_host: String,
    /// 额度查询 host —— ★ **两个服务商都是 `zcode.z.ai`**，见 [`QUOTA_HOST`]。
    pub quota_host: String,
}

/// 凭证里的 provider 别名 → [`Provider`]。
///
/// 大小写不敏感、前后空白忽略；认不出返回 [`Provider::Unknown`]（不猜）。
pub fn parse_provider(s: &str) -> Provider {
    match s.trim().to_ascii_lowercase().as_str() {
        "zai" | "z.ai" | "z-ai" | "z_ai" => Provider::Zai,
        "bigmodel" | "big-model" | "big_model" | "zhipu" | "智谱" | "glm" => Provider::Bigmodel,
        _ => Provider::Unknown,
    }
}

/// 从 `Auth::domain` 反解服务商。
pub fn provider_of_domain(domain: &str) -> Provider {
    let d = domain.trim().to_ascii_lowercase();
    if d.is_empty() {
        return Provider::Unknown;
    }
    if d.contains("bigmodel") || d.contains("zhipu") {
        return Provider::Bigmodel;
    }
    if d.contains("z.ai") || d.contains("zai") {
        return Provider::Zai;
    }
    Provider::Unknown
}

/// 服务商 → 承载它的域名（写回 `Auth::domain` 用）。
pub fn domain_of_provider(p: Provider) -> &'static str {
    match p {
        Provider::Bigmodel => "open.bigmodel.cn",
        _ => "api.z.ai",
    }
}

/// start-plan 通道的 base（★ 与 API Key 通道的端点**不能混用**）。
///
/// 从官方客户端自己的配置读到（`~/.zcode/v2/config.json`）：
/// `builtin:bigmodel-start-plan: kind="anthropic"; apiKey=eyJ…（就是凭证文件里那个 jwt）;
/// baseURL=https://zcode.z.ai/api/v1/zcode-plan/anthropic`。
///
/// 而 [`Endpoints::anthropic_base`] 里那两个（`open.bigmodel.cn/api/anthropic`、
/// `api.z.ai/api/anthropic`）是 **API Key 通道**的端点，不是 start-plan 的。
/// 两者不能混用：start-plan 通道凭证是 **jwt**、额度挂在 start-plan 上；
/// coding/paas 通道凭证是 `{apiKey}.{secret}`、**没有该账号的资源包**
/// （实测恒回 429 code=1113）。用户看到的现象是「明明有 2.99 亿额度却报余额不足」，
/// 根因就是**拿到了 start-plan 的额度，却把对话发到了 coding/paas 通道**。
pub const START_PLAN_BASE: &str = "https://zcode.z.ai/api/v1/zcode-plan/anthropic";

/// Anthropic Messages 路径。
///
/// ⚠ 完整路径要在 base 之后再拼；实测直接打 base 是 404。
pub const ANTHROPIC_MESSAGES_PATH: &str = "/v1/messages";

/// 额度查询 host。
///
/// ★★ **必须**与 [`Endpoints::biz_host`] 分开。最初让额度查询复用 `biz_host`，
/// 于是 bigmodel 账号发去了 `open.bigmodel.cn/api/v1/zcode-plan/billing/balance`
/// ⇒ 上游回 `HTTP 200 {"code":500,"msg":"404 NOT_FOUND"}`。那个响应**是 200**，
/// 故不会被 `status >= 400` 拦下，会被当成「拿到了数据但额度为空」⇒ 界面显示
/// 「额度 0」/「未知」，而用户明明有 3 亿 token ——
/// **这类「错误被包装成成功」的失败最难发现**。
///
/// 实测三 host 对照（`uitest/diag-balance-endpoint.cjs`）：
/// `zcode.z.ai` → code=0 完整额度 ✓；`open.bigmodel.cn` → code=500 404 NOT_FOUND ✗；
/// `api.z.ai` → code=500 404 NOT_FOUND ✗。即**两个服务商的额度查询都走
/// `zcode.z.ai`** —— 那是统一的账单网关，与凭证兑换（各服务商自己的 host）
/// 不是一回事。
pub const QUOTA_HOST: &str = "https://zcode.z.ai";

/// 全部**真实**服务商（不含 `Unknown`）。
pub fn all_providers() -> [Provider; 2] {
    [Provider::Zai, Provider::Bigmodel]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_provider_accepts_aliases_case_insensitively() {
        for s in ["zai", "ZAI", " z.ai ", "z-ai", "Z_AI"] {
            assert_eq!(parse_provider(s), Provider::Zai, "输入 {s:?}");
        }
        for s in [
            "bigmodel",
            "BigModel",
            "big-model",
            "big_model",
            "zhipu",
            "智谱",
            "GLM",
        ] {
            assert_eq!(parse_provider(s), Provider::Bigmodel, "输入 {s:?}");
        }
        for s in ["", "  ", "openai", "anthropic", "unknown"] {
            assert_eq!(parse_provider(s), Provider::Unknown, "输入 {s:?}");
        }
    }

    #[test]
    fn endpoints_quota_host_is_always_zcode_z_ai() {
        // ★ 这条是防「bigmodel 账号把额度查询发去 open.bigmodel.cn」那个
        // 「HTTP 200 包着 404」的静默失败复活。
        for p in all_providers() {
            let e = p.endpoints();
            assert_eq!(
                e.quota_host, "https://zcode.z.ai",
                "{p:?} 的 quota_host 必须是统一账单网关"
            );
        }
        let bm = Provider::Bigmodel.endpoints();
        assert_eq!(bm.openai_base, "https://open.bigmodel.cn/api/coding/paas/v4");
        assert_eq!(bm.anthropic_base, "https://open.bigmodel.cn/api/anthropic");
        assert_eq!(bm.biz_host, "https://open.bigmodel.cn");

        let zai = Provider::Zai.endpoints();
        assert_eq!(zai.openai_base, "https://api.z.ai/api/coding/paas/v4");
        assert_eq!(zai.anthropic_base, "https://api.z.ai/api/anthropic");
        assert_eq!(zai.biz_host, "https://api.z.ai");

        // Unknown 与 Zai 同端点（有意）。
        assert_eq!(Provider::Unknown.endpoints(), zai);
    }

    #[test]
    fn start_plan_base_is_not_the_api_key_channel() {
        assert_eq!(
            START_PLAN_BASE,
            "https://zcode.z.ai/api/v1/zcode-plan/anthropic"
        );
        for p in all_providers() {
            assert_ne!(
                p.endpoints().anthropic_base,
                START_PLAN_BASE,
                "{p:?}：start-plan 与 API Key 通道不能混用"
            );
        }
        assert_eq!(ANTHROPIC_MESSAGES_PATH, "/v1/messages");
    }

    #[test]
    fn provider_domain_round_trip() {
        assert_eq!(domain_of_provider(Provider::Bigmodel), "open.bigmodel.cn");
        assert_eq!(domain_of_provider(Provider::Zai), "api.z.ai");
        assert_eq!(domain_of_provider(Provider::Unknown), "api.z.ai");

        for p in all_providers() {
            assert_eq!(
                provider_of_domain(domain_of_provider(p)),
                p,
                "{p:?} 的 domain 反解必须回到自身"
            );
        }
    }

    #[test]
    fn provider_of_domain_matches_substrings() {
        assert_eq!(provider_of_domain("open.bigmodel.cn"), Provider::Bigmodel);
        assert_eq!(provider_of_domain("BIGMODEL.CN"), Provider::Bigmodel);
        assert_eq!(provider_of_domain("zhipu.example"), Provider::Bigmodel);
        assert_eq!(provider_of_domain("api.z.ai"), Provider::Zai);
        assert_eq!(provider_of_domain("zcode.z.ai"), Provider::Zai);
        assert_eq!(provider_of_domain("zaicorp.io"), Provider::Zai);
        assert_eq!(provider_of_domain(""), Provider::Unknown);
        assert_eq!(provider_of_domain("   "), Provider::Unknown);
        assert_eq!(provider_of_domain("example.com"), Provider::Unknown);
    }

    #[test]
    fn labels_and_strings_are_stable() {
        // 落盘用 as_str（Unknown = 空串），界面用 label。
        assert_eq!(Provider::Zai.as_str(), "zai");
        assert_eq!(Provider::Bigmodel.as_str(), "bigmodel");
        assert_eq!(Provider::Unknown.as_str(), "");
        assert_eq!(Provider::Zai.label(), "Z.AI");
        assert_eq!(Provider::Bigmodel.label(), "智谱");
        assert_eq!(Provider::Unknown.label(), "未指定");
        assert_eq!(parse_provider(Provider::Bigmodel.as_str()), Provider::Bigmodel);
    }
}
