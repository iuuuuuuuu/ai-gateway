//! 模型名里的**路由前缀协议**：`[产品:][区域:]模型名`。
//!
//! 对应 Go 源文件 `internal/server/resolve_model.go`（逐字行为移植）。
//!
//! # 为什么需要它（所有者的需求）
//!
//! 三个平台**有重名模型** —— `glm-5.3` 既在 ZCode 的套餐里、也在 WorkBuddy
//! 的清单里。用户输入裸名时，账号池按「最早到期分层」选号，可能选中一个
//! **没有该模型资源包**的账号（ZCode 那条通道回 `1113 无可用资源包`，
//! 用户看到的是"余额不足"）。客户端用前缀显式指定平台/区域即可消除歧义。
//!
//! # 三条容易被改坏的硬约束
//!
//! 1. **无前缀时 product/realm 是空串，不是 `"cn"`** —— 空串表示「不限制」，
//!    默认成 `cn` 会让所有国际版账号**静默从候选池里消失**。
//! 2. **未知前段必须原样保留** —— `deepseek:v3` 里的 `deepseek` 不是前缀，
//!    剥掉它就是把模型名本身吃掉（上游回「模型不存在」，而原因在网关）。
//! 3. **最多剥两层，且产品/区域顺序可颠倒** —— `global:qoder:x` 与
//!    `qoder:global:x` 等价。

use crate::auth::Region;

/// 产品前缀常量：WorkBuddy / QoderWork / ZCode（**复用** `auth` 包，不重新定义）。
///
/// 三者最终会与账号的 [`crate::auth::Auth::product_of`] 比较，各写一份字面量
/// 迟早会分叉，而那种错误**不会编译失败**（两边都是字符串），只会表现为
/// 「该平台永远选不中」。故这里只做**重导出**：
/// `resolve_model::PRODUCT_QODER` 与 [`crate::auth::PRODUCT_QODER`] 是**同一个**
/// 常量，而不是值相等的两份副本。
pub use crate::auth::{PRODUCT_QODER, PRODUCT_TRAE, PRODUCT_WORKBUDDY, PRODUCT_ZCODE};

/// 区域前缀：国服（英文写法）。
pub const REALM_CN: &str = "cn";

/// 区域前缀：国际版（英文写法）。
pub const REALM_GLOBAL: &str = "global";

/// 区域前缀：国际版（中文别名）。
///
/// 所有者习惯写「国际版」—— 要求他改写成英文 `global` 是**改他的需求**，
/// 而不是满足它。两种写法都接受，成本仅一个枚举项。
pub const REALM_INTL_CN: &str = "国际版";

/// 区域前缀：国服（中文别名）。
///
/// ⚠ 这不是可有可无的"锦上添花"：`/v1/models` 的 aliases 里**我们下发的
/// 组合名用的就是「国服」**（`realmLabelOf(RegionCN)` 的返回值）。
/// 不接受它，用户从界面抄下来的写法会被当成裸模型名 ⇒ 上游 `11102`。
pub const REALM_CN_ALT: &str = "国服";

/// 区域前缀：国服（中文别名，所有者本人的口头说法）。
///
/// 2026-09-20 所有者原话是「平台:国内:模型名」，一并接受，让他抄哪个都能用。
pub const REALM_CN_ALT2: &str = "国内";

/// 模型名前缀协议的解析结果。
///
/// 字段顺序与 Go 侧返回值 `(product, realm, bare)` 一致 —— 那边是**真实踩过
/// 的坑**：写成 `model, realm := resolveModel(...)` 会静默交换，表现为
/// 「单一模型锁定」报「收到的是 (未指定)」，而该缺陷**不会导致编译错误**。
/// Rust 侧用具名字段消除了这个风险，但仍保留同一顺序以便对照。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedModel {
    /// 产品前缀（[`PRODUCT_WORKBUDDY`] / [`PRODUCT_QODER`] / [`PRODUCT_ZCODE`]）；空串 = 不限制。
    pub product: String,
    /// 区域前缀（[`REALM_CN`] / [`REALM_GLOBAL`]）；空串 = 不限制。
    pub realm: String,
    /// 剥掉前缀后的裸模型名。
    pub bare: String,
}

/// 解析模型名里的路由前缀协议：`[产品:][区域:]模型名`。
///
/// # 协议（区域前缀大小写敏感，与既有行为一致）
///
/// ```text
/// "glm-5.3"                      → product="", realm="", bare=glm-5.3（不限制）
/// "cn:glm-5.3"                   → realm=cn,       bare=glm-5.3
/// "global:glm-5.3"               → realm=global,   bare=glm-5.3
/// "国际版:glm-5.3"                → realm=global,   bare=glm-5.3（中文别名）
/// "qoder:glm-5.3"                → product=qoder,  bare=glm-5.3
/// "qoder:global:glm-5.3"         → product=qoder, realm=global, bare=glm-5.3
/// "zcode:国际版:glm-5.3"          → product=zcode, realm=global, bare=glm-5.3
/// "deepseek:v3"                  → 都不匹配，原样返回（前段不在枚举内）
/// ```
///
/// # 剥离规则
///
/// 最多剥**两层**（产品 + 区域），顺序固定先产品后区域；反过来的
/// `global:qoder:x` 也接受。产品段只在 product 尚未确定时接受，区域段同理。
///
/// 任何一段**既不是产品也不是区域**时**立刻停止**，把已累积的前缀与**原样
/// 剩余串**一起返回。⚠ 必须立刻停：`deepseek:v3` 里的 `deepseek` 不是前缀，
/// 若继续往下剥会把模型名本身吃掉 —— 那正是「前段不在枚举内，不剥离」
/// 这条既有约定要防的事。
///
/// # 为什么无前缀时返回空串而非 `"cn"`
///
/// 空串表示**不限制**，保持既有行为（老客户端不带前缀，不应因此改变选号范围）。
/// 这一点很重要：若默认成 `cn`，所有国际版账号会**静默从候选里消失**。
///
/// # 为什么用 `split_once` 而不是 `split(':')`
///
/// 只有**第一个**冒号是分隔符，tail 必须保留后续所有冒号（`cn:a:b` 的
/// 裸名是 `a:b`，不是 `a`）。[`str::split_once`] 正好在首个匹配处切一次，
/// 且不涉及可 panic 的字符串索引。
pub fn resolve_model(model: &str) -> ResolvedModel {
    let mut product = String::new();
    let mut realm = String::new();
    let mut rest = model;

    // 最多剥两层前缀（产品 + 区域）。
    for _ in 0..2 {
        let (prefix, tail) = match rest.split_once(':') {
            Some(parts) => parts,
            None => break,
        };

        if product.is_empty() && is_product_prefix(prefix) {
            product = normalize_product(prefix);
            rest = tail;
        } else if realm.is_empty() && is_realm_prefix(prefix) {
            realm = normalize_realm(prefix);
            rest = tail;
        } else {
            // 前段既不是产品也不是区域 —— 整串原样保留，立刻停。
            return ResolvedModel {
                product,
                realm,
                bare: rest.to_string(),
            };
        }
    }

    ResolvedModel {
        product,
        realm,
        bare: rest.to_string(),
    }
}

/// 判断某段是否是**产品**前缀。
///
/// 只认 [`PRODUCT_WORKBUDDY`] / [`PRODUCT_QODER`] / [`PRODUCT_ZCODE`] 三个，
/// **不接受任何别名** —— 产品标识是机器用的，不该有歧义。
///
/// 大小写不敏感（`Qoder:` / `QODER:` 都接受），与 Go 侧 `strings.ToLower`
/// 同口径。
pub fn is_product_prefix(s: &str) -> bool {
    let lower = s.to_lowercase();
    [PRODUCT_WORKBUDDY, PRODUCT_QODER, PRODUCT_ZCODE, PRODUCT_TRAE].contains(&lower.as_str())
}

/// 判断某段是否是**区域**前缀（**大小写敏感**）。
///
/// 接受英文与中文别名：
///
/// ```text
/// cn / 国服 / 国内   → 国服
/// global / 国际版    → 国际版
/// ```
///
/// # ⚠ 「国服」「国内」是必须接受的（2026-09-20 实测缺陷）
///
/// `/v1/models` 的 `aliases` 里，**我们自己下发**的组合名用的就是「国服」。
/// 本函数若只认 `cn`，用户从界面复制 `workbuddy:国服:xxx` 填进配置后会
/// **解析失败**：区域段不被识别 ⇒ 整段被当成裸模型名 ⇒ 上游回
/// `11102「模型不存在」`。
///
/// 这是**自相矛盾**：我们给的写法自己读不回来。所有者 2026-09-20 反馈的
/// 「接口返回的还是没有 平台:国内:模型名」正指向这条链路。
///
/// 「国内」一并接受：所有者的原话就是「国内」，口头与书面都有用；
/// 宽容输入（接受多种等价写法）在这里是对的 —— 用户抄哪个都能用。
///
/// 与产品前缀不同，这里**不做大小写归一**：区域是短英文词，容易与模型名
/// 混淆（`Cn:glm` 不该被当成前缀）。
pub fn is_realm_prefix(s: &str) -> bool {
    [REALM_CN, REALM_GLOBAL, REALM_INTL_CN, REALM_CN_ALT, REALM_CN_ALT2].contains(&s)
}

/// 归一化产品前缀为小写标识（去空白 + 转小写）。
///
/// 产品名允许任意大小写（`Qoder:` / `QODER:` 都接受）—— 与区域前缀的
/// 「大小写敏感」不同：区域是短英文词，容易与模型名混淆；产品名是明确的
/// 品牌词，宽容更友好，且不会与模型名撞。
pub fn normalize_product(s: &str) -> String {
    s.trim().to_lowercase()
}

/// 归一化区域前缀为内部枚举值（[`REALM_CN`] / [`REALM_GLOBAL`]）。
///
/// 不归一的话 [`realm_to_region`] 会走 default 分支返回 [`Region::Any`] ——
/// 用户的「只要国服」会**静默变成「不限区域」**，比报错更难发现。
pub fn normalize_realm(s: &str) -> String {
    match s {
        REALM_INTL_CN => REALM_GLOBAL.to_string(),
        REALM_CN_ALT | REALM_CN_ALT2 => REALM_CN.to_string(),
        other => other.to_string(),
    }
}

/// 把区域前缀映射成 [`Region`]。
///
/// 空串 / 未知 → [`Region::Any`]（不限制）。这样调用方不必自己判空 ——
/// 直接把结果赋给 `prefer_region` 即可，无前缀时自然保持「不限制」。
///
/// ⚠ 入参应当是 [`normalize_realm`] **归一后**的值：中文别名在这里走
/// default 分支。归一与映射分两处是刻意的，各自可单测（见 `resolve_model`
/// 与 `normalize_realm` 的测试）。
pub fn realm_to_region(realm: &str) -> Region {
    match realm {
        REALM_CN => Region::CN,
        REALM_GLOBAL => Region::Intl,
        _ => Region::Any,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth;

    /// 三字段一并断言的公共夹具，对应 Go 侧每个表驱动测试的循环体。
    fn check(cases: &[(&str, &str, &str, &str)]) {
        for (input, product, realm, bare) in cases {
            let got = resolve_model(input);
            let (gp, gr, gb) = (
                got.product.as_str(),
                got.realm.as_str(),
                got.bare.as_str(),
            );
            assert_eq!(
                (gp, gr, gb),
                (*product, *realm, *bare),
                "resolve_model({:?}) = ({:?},{:?},{:?}) 期望 ({:?},{:?},{:?})",
                input,
                gp,
                gr,
                gb,
                product,
                realm,
                bare
            );
        }
    }

    /// 区域前缀（既有协议，必须保持兼容）。对应 `TestResolveModelPrefix`。
    #[test]
    fn resolve_model_prefix() {
        check(&[
            ("cn:glm-5.2", "", "cn", "glm-5.2"),
            ("global:gpt-5.4", "", "global", "gpt-5.4"),
            // 无前缀 = 不限制（保持既有行为）
            ("glm-5.2", "", "", "glm-5.2"),
            // 前段不在枚举内：不剥离（模型名本身可能含冒号）
            ("deepseek:v3", "", "", "deepseek:v3"),
            // 区域前缀大小写敏感：不做归一（与既有行为一致）
            ("GLOBAL:gpt-5", "", "", "GLOBAL:gpt-5"),
            ("Cn:glm", "", "", "Cn:glm"),
        ]);
    }

    /// 产品前缀。对应 `TestResolveModelProductPrefix`。
    #[test]
    fn resolve_model_product_prefix() {
        check(&[
            ("qoder:glm-5.3", "qoder", "", "glm-5.3"),
            ("zcode:glm-5.3", "zcode", "", "glm-5.3"),
            ("workbuddy:deepseek-v4.1-flash", "workbuddy", "", "deepseek-v4.1-flash"),
            // 产品名允许大小写（品牌词，不会与模型名撞）
            ("Qoder:glm-5.3", "qoder", "", "glm-5.3"),
            ("ZCODE:glm-5.3", "zcode", "", "glm-5.3"),
        ]);
    }

    /// 产品 + 区域**两段**（所有者要的完整写法）。
    ///
    /// 这是他的原话「平台:国际版:模型名」对应的形态。
    /// 对应 `TestResolveModelProductAndRealm`。
    #[test]
    fn resolve_model_product_and_realm() {
        check(&[
            ("qoder:global:glm-5.3", "qoder", "global", "glm-5.3"),
            ("zcode:cn:glm-5.3", "zcode", "cn", "glm-5.3"),
            ("workbuddy:global:deepseek-v4.1-flash", "workbuddy", "global", "deepseek-v4.1-flash"),
            // 顺序反了也应接受（用户不该被"必须先产品后区域"绊住）
            ("global:qoder:glm-5.3", "qoder", "global", "glm-5.3"),
        ]);
    }

    /// 中文别名「国际版」。对应 `TestResolveModelChineseRealmAlias`。
    #[test]
    fn resolve_model_chinese_realm_alias() {
        check(&[
            ("国际版:glm-5.3", "", "global", "glm-5.3"),
            ("qoder:国际版:glm-5.3", "qoder", "global", "glm-5.3"),
            ("zcode:国际版:glm-5.3", "zcode", "global", "glm-5.3"),
        ]);
    }

    /// 边界：空串、仅冒号、空前缀、空裸名。对应 `TestResolveModelEdge`。
    #[test]
    fn resolve_model_edge() {
        check(&[
            ("", "", "", ""),
            // 空前缀不匹配任何枚举
            (":", "", "", ":"),
            (":model", "", "", ":model"),
            // 前缀合法 + 空裸名仍剥离
            ("cn:", "", "cn", ""),
            ("global:", "", "global", ""),
            ("qoder:", "qoder", "", ""),
            // 只剥一层区域
            ("cn:a:b", "", "cn", "a:b"),
            // 剥两层后剩下的原样保留
            ("qoder:cn:a:b", "qoder", "cn", "a:b"),
        ]);
    }

    /// 未知前缀**绝不**被剥掉。对应 `TestResolveModelDoesNotEatUnknownPrefix`。
    ///
    /// # 这条守的是什么
    ///
    /// 循环剥前缀若写成"无脑按冒号切分"，`deepseek:v3` 会被剥成 `v3` ——
    /// 模型名被吃掉，用户看到的是"模型不存在"，而原因在网关。
    /// 既有注释明确写了「前段不在枚举内，不剥离」，本条把它钉死。
    #[test]
    fn resolve_model_does_not_eat_unknown_prefix() {
        for input in [
            "deepseek:v3",
            "foo:bar",
            "some-vendor:model:v2",
            "a:b:c",
        ] {
            let got = resolve_model(input);
            assert_eq!(
                (got.product.as_str(), got.realm.as_str(), got.bare.as_str()),
                ("", "", input),
                "resolve_model({:?})：未知前缀必须原样保留，不能吃掉模型名",
                input
            );
        }
    }

    /// 锁住字段语义 `product` / `realm` / `bare`。对应
    /// `TestResolveModelReturnsProductFirst`。
    ///
    /// Go 侧那里守的是**返回值顺序**（`model, realm := resolveModel(...)`
    /// 会静默交换，且不编译报错）。Rust 用具名字段消除了顺序风险，本条改为
    /// 钉住「哪个字段装哪个值」，防止后续有人把字段含义对调。
    #[test]
    fn resolve_model_returns_product_first() {
        let got = resolve_model("qoder:global:glm-5.3");
        assert_eq!(got.product, "qoder", "product 字段应是产品（qoder）");
        assert_eq!(got.realm, "global", "realm 字段应是区域（global）");
        assert_eq!(got.bare, "glm-5.3", "bare 字段应是裸模型名（glm-5.3）");
    }

    /// 区域前缀 → [`Region`] 的映射。对应 `TestRealmToRegion`。
    ///
    /// 空串必须映射成 [`Region::Any`]（不限制）—— 若误映射成 `CN`，
    /// 所有国际版账号会**静默从候选里消失**（用户看到"没有可用账号"，
    /// 而他明明有国际版号）。
    ///
    /// ⚠ 中文别名**不**在这里接受：必须先过 [`normalize_realm`]。
    #[test]
    fn realm_to_region_maps_known_realms() {
        assert_eq!(realm_to_region(""), Region::Any);
        assert_eq!(realm_to_region("cn"), Region::CN);
        assert_eq!(realm_to_region("global"), Region::Intl);
        assert_eq!(realm_to_region("bogus"), Region::Any);
        // 中文别名在映射层走 default —— 这是刻意的，归一在 normalize_realm。
        assert_eq!(realm_to_region(REALM_CN_ALT), Region::Any);
    }

    /// 产品前缀常量必须与 `auth` 包一致。对应
    /// `TestProductPrefixConstantsMatchAuth`。
    ///
    /// 它们最终会与账号的 `product_of()` 比较 —— 拼错就是"永远选不中"，
    /// 而那种错误**不会编译失败**（两边都是字符串）。
    #[test]
    fn product_prefix_constants_match_auth() {
        let pairs = [
            (PRODUCT_WORKBUDDY, auth::PRODUCT_WORKBUDDY),
            (PRODUCT_QODER, auth::PRODUCT_QODER),
            (PRODUCT_ZCODE, auth::PRODUCT_ZCODE),
        ];
        for (ours, theirs) in pairs {
            assert_eq!(
                ours, theirs,
                "产品常量不一致：resolve_model 包 {:?} vs auth 包 {:?} —— 会导致该平台永远选不中",
                ours, theirs
            );
        }
    }

    /// 我们**下发**的组合名必须能被自己解析。对应
    /// `TestResolveModelAcceptsOurOwnGeneratedAliases`。
    ///
    /// # 为什么这条必须有（2026-09-20 实测缺陷）
    ///
    /// `/v1/models` 的 `aliases` 里下发的国服写法是「国服」。而
    /// [`is_realm_prefix`] 此前只认 `cn` —— 于是用户从界面复制
    /// `workbuddy:国服:xxx` 填进配置后：
    ///
    /// ```text
    /// 区域段不被识别 ⇒ 整段被当成**裸模型名** ⇒ 上游 11102「模型不存在」
    /// ```
    ///
    /// 自相矛盾：**我们给的写法自己读不回来**。
    ///
    /// 判据用「解析出来的 bare 必须等于真模型名」—— 若区域段没被剥掉，
    /// bare 会变成 `国服:glm-5.3`（含前缀），断言即失败。
    #[test]
    fn resolve_model_accepts_our_own_generated_aliases() {
        check(&[
            // 我们下发的三种组合（见 capability.go 的 mergedModelList）
            ("workbuddy:国服:glm-5.3", PRODUCT_WORKBUDDY, REALM_CN, "glm-5.3"),
            ("workbuddy:国际版:glm-5.3", PRODUCT_WORKBUDDY, REALM_GLOBAL, "glm-5.3"),
            // 所有者的说法「国内」—— 宽容输入，一并接受
            ("workbuddy:国内:glm-5.3", PRODUCT_WORKBUDDY, REALM_CN, "glm-5.3"),
            ("qoder:国服:Qwen3.8-Flash", PRODUCT_QODER, REALM_CN, "Qwen3.8-Flash"),
            // 纯区域前缀（不带产品）
            ("国服:glm-5.3", "", REALM_CN, "glm-5.3"),
            ("国内:glm-5.3", "", REALM_CN, "glm-5.3"),
            // 英文仍照旧
            ("cn:glm-5.3", "", REALM_CN, "glm-5.3"),
        ]);
    }

    /// 中文别名要归一到内部枚举值。对应
    /// `TestNormalizeRealmMapsChineseAliases`。
    ///
    /// 不归一的话 [`realm_to_region`] 会走 default 分支返回 [`Region::Any`]
    /// —— 用户的"只要国服"会**静默变成"不限区域"**，比报错更难发现。
    #[test]
    fn normalize_realm_maps_chinese_aliases() {
        assert_eq!(normalize_realm(REALM_CN_ALT), REALM_CN);
        assert_eq!(normalize_realm(REALM_CN_ALT2), REALM_CN);
        assert_eq!(normalize_realm(REALM_INTL_CN), REALM_GLOBAL);
        assert_eq!(normalize_realm(REALM_CN), REALM_CN);
        assert_eq!(normalize_realm(REALM_GLOBAL), REALM_GLOBAL);

        // 归一后必须真的映射到对应区域（不是 RegionAny）
        assert_eq!(
            realm_to_region(&normalize_realm(REALM_CN_ALT)),
            Region::CN,
            "「国服」应映射到 RegionCN"
        );
        assert_eq!(
            realm_to_region(&normalize_realm(REALM_CN_ALT2)),
            Region::CN,
            "「国内」应映射到 RegionCN"
        );
        assert_eq!(
            realm_to_region(&normalize_realm(REALM_INTL_CN)),
            Region::Intl,
            "「国际版」应映射到 RegionIntl"
        );
    }

    /// 产品前缀谓词：大小写宽容、**不接受别名**、不吞中文。
    ///
    /// Go 侧没有直接对应用例（`isProductPrefix` 只被间接覆盖），
    /// 但用户明确要求「no aliases」，故在此钉死。
    #[test]
    fn is_product_prefix_is_case_insensitive_without_aliases() {
        for yes in ["workbuddy", "qoder", "zcode", "Qoder", "QODER", "ZCode", "WorkBuddy"] {
            assert!(is_product_prefix(yes), "{:?} 应被认成产品前缀", yes);
        }
        for no in [
            "", "wb", "work", "work-buddy", "workbuddy2", "qoderwork", "z-code", "国际版", "cn",
            " global",
        ] {
            assert!(!is_product_prefix(no), "{:?} 不应被认成产品前缀", no);
        }
    }

    /// `normalize_product` = 去空白 + 转小写。
    #[test]
    fn normalize_product_trims_and_lowercases() {
        assert_eq!(normalize_product("Qoder"), "qoder");
        assert_eq!(normalize_product("ZCODE"), "zcode");
        assert_eq!(normalize_product("  workbuddy  "), "workbuddy");
        assert_eq!(normalize_product(""), "");
    }
}
