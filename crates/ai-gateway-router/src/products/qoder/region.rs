//! Qoder 服务区域与端点表。
//!
//! 对应 Go 源文件 `internal/qoder/region.go`（123 行）。
//!
//! # 为什么区域必须是三态而不是布尔
//!
//! 凭证里**可能没有** `domain` 字段（手写凭证、旧版导入）。此时把区域猜成
//! 国服会让国际版账号打到国服端点 —— 必然失败，且失败文案与真实原因无关。
//! 故 [`QoderRegion::Unknown`] 是一等状态：**未知就不猜**，由界面让用户确认。
//!
//! ⚠ 但 [`QoderRegion::endpoints`] 对 Unknown **回落国服**（与 Go `endpointsOf`
//! 的 default 分支逐字一致）—— 那是"没有更好选择时的可用默认"，与"解析时
//! 不猜"不矛盾：解析层保持 Unknown，调用方（界面）据此提示用户。

/// Qoder 服务区域。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum QoderRegion {
    /// 国服（`qoder.com.cn`）。
    CN,
    /// 国际版（授权页 `qoder.com`、API `qoder.sh`）。
    Intl,
    /// 未指定 —— **不猜**。
    #[default]
    Unknown,
}

/// 一个区域的三个端点。
///
/// ★ 国际版的**授权页在 `qoder.com` 而 API 在 `qoder.sh`**（不同域，不要统一；
/// `qoder.sh` 的网页不存在，DNS 都不解析）。国服三者同域。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// 网页 / OAuth 授权页基址。
    pub website: String,
    /// OpenAPI 基址（额度、用户信息、令牌刷新 —— 只需 Bearer，不签名）。
    pub openapi: String,
    /// Gateway 基址（模型清单、对话 —— **必须 COSY 签名**）。
    pub gateway: String,
}

impl QoderRegion {
    /// 与 Go `Region` 的字面量一致：`"cn"` / `"intl"` / `""`。
    pub fn as_str(self) -> &'static str {
        match self {
            QoderRegion::CN => "cn",
            QoderRegion::Intl => "intl",
            QoderRegion::Unknown => "",
        }
    }

    /// 该区域写回凭证的 `domain` 值。
    ///
    /// 国际版是 `qoder.sh`（API 域），不是授权页的 `qoder.com` —— 与 Go
    /// `Region.Domain()` 逐字一致。
    pub fn domain(self) -> &'static str {
        match self {
            QoderRegion::Intl => "qoder.sh",
            // ★ default（含 CN 与 Unknown）→ 国服。
            _ => "qoder.com.cn",
        }
    }

    /// 面向界面的中文名。
    pub fn label(self) -> &'static str {
        match self {
            QoderRegion::Intl => "国际版",
            QoderRegion::CN => "国服",
            QoderRegion::Unknown => "未指定",
        }
    }

    /// 端点表。
    pub fn endpoints(self) -> Endpoints {
        match self {
            QoderRegion::Intl => Endpoints {
                website: "https://qoder.com".to_string(),
                openapi: "https://openapi.qoder.sh".to_string(),
                gateway: "https://api3.qoder.sh".to_string(),
            },
            // ★ default（含 CN 与 Unknown）→ 国服端点。
            _ => Endpoints {
                website: "https://qoder.com.cn".to_string(),
                openapi: "https://openapi.qoder.com.cn".to_string(),
                gateway: "https://gateway.qoder.com.cn".to_string(),
            },
        }
    }

    /// 授权页基址。
    pub fn website(self) -> String {
        self.endpoints().website
    }

    /// OpenAPI 基址。
    pub fn openapi(self) -> String {
        self.endpoints().openapi
    }

    /// Gateway 基址。
    pub fn gateway(self) -> String {
        self.endpoints().gateway
    }
}

/// 从 `domain` 字段判定区域。
///
/// 与 Go `RegionFromDomain` 逐字一致（含判据顺序）：
///
/// ```text
/// ""                     → Unknown
/// *.com.cn / qoder.com.cn → CN
/// *.sh     / qoder.sh     → Intl
/// qoder.com               → Intl
/// 其它                     → Unknown
/// ```
///
/// ⚠ 判据顺序有意义：`.com.cn` 必须在 `.com` 之前判（否则 `qoder.com.cn`
/// 会先被 `.com` 规则抢走 —— 虽然 `HasSuffix("qoder.com.cn", "qoder.com")`
/// 恰好为假，但换个域名就会踩到）。
pub fn region_from_domain(domain: &str) -> QoderRegion {
    let d = domain.trim().to_lowercase();
    if d.is_empty() {
        return QoderRegion::Unknown;
    }
    if d.ends_with(".com.cn") || d.ends_with("qoder.com.cn") {
        return QoderRegion::CN;
    }
    if d.ends_with(".sh") || d.ends_with("qoder.sh") {
        return QoderRegion::Intl;
    }
    if d.ends_with("qoder.com") || d == "qoder.com" {
        return QoderRegion::Intl;
    }
    QoderRegion::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_from_domain_boundaries() {
        // 空 / 空白 → 未知（不猜）。
        assert_eq!(region_from_domain(""), QoderRegion::Unknown);
        assert_eq!(region_from_domain("   "), QoderRegion::Unknown);

        // 国服。
        assert_eq!(region_from_domain("qoder.com.cn"), QoderRegion::CN);
        assert_eq!(region_from_domain("gateway.qoder.com.cn"), QoderRegion::CN);
        assert_eq!(region_from_domain("api.foo.com.cn"), QoderRegion::CN);

        // 国际版：授权页域与 API 域**都要**认成 Intl。
        assert_eq!(region_from_domain("qoder.com"), QoderRegion::Intl);
        assert_eq!(region_from_domain("api3.qoder.sh"), QoderRegion::Intl);
        assert_eq!(region_from_domain("qoder.sh"), QoderRegion::Intl);
        assert_eq!(region_from_domain("foo.sh"), QoderRegion::Intl);

        // 大小写与首尾空白要归一。
        assert_eq!(region_from_domain("  QODER.COM.CN "), QoderRegion::CN);
        assert_eq!(region_from_domain("Qoder.Sh"), QoderRegion::Intl);

        // ★ 不认识的域 → Unknown（**不是** CN）。
        assert_eq!(region_from_domain("example.com"), QoderRegion::Unknown);
        assert_eq!(region_from_domain("qoder.cn"), QoderRegion::Unknown);
        // 子串不是后缀，不算命中。
        assert_eq!(region_from_domain("qoder.com.cn.evil.com"), QoderRegion::Unknown);
    }

    #[test]
    fn endpoints_and_labels_match_go() {
        let cn = QoderRegion::CN.endpoints();
        assert_eq!(cn.website, "https://qoder.com.cn");
        assert_eq!(cn.openapi, "https://openapi.qoder.com.cn");
        assert_eq!(cn.gateway, "https://gateway.qoder.com.cn");

        let intl = QoderRegion::Intl.endpoints();
        // ★ 国际版授权页在 qoder.com、API 在 qoder.sh —— 不要"统一"。
        assert_eq!(intl.website, "https://qoder.com");
        assert_eq!(intl.openapi, "https://openapi.qoder.sh");
        assert_eq!(intl.gateway, "https://api3.qoder.sh");

        // Unknown 回落国服端点（有意的默认），但标签仍是"未指定"。
        assert_eq!(QoderRegion::Unknown.endpoints(), cn);
        assert_eq!(QoderRegion::Unknown.label(), "未指定");
        assert_eq!(QoderRegion::CN.label(), "国服");
        assert_eq!(QoderRegion::Intl.label(), "国际版");

        assert_eq!(QoderRegion::CN.as_str(), "cn");
        assert_eq!(QoderRegion::Intl.as_str(), "intl");
        assert_eq!(QoderRegion::Unknown.as_str(), "");
    }

    #[test]
    fn domain_round_trips_to_region() {
        // `domain()` 写回凭证后必须能被 `region_from_domain` 还原。
        for r in [QoderRegion::CN, QoderRegion::Intl] {
            assert_eq!(region_from_domain(r.domain()), r);
        }
    }
}
