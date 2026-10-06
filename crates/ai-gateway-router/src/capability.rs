//! 能力索引：`模型名 × 区域 → 能力/倍率` 的真值表，以及 `/v1/models` 的目录合并。
//!
//! 对应 Go 侧（`wt-port/go-gateway/`）：
//!
//! | 本文件                                        | Go 源文件                                        |
//! |-----------------------------------------------|--------------------------------------------------|
//! | [`RegionCapability`]                          | `internal/server/capability.go` `regionCapability` |
//! | [`CapabilityIndex`] / [`CapabilityIndex::build`] | 同上 `capabilityIndex` / `buildCapabilityIndex` |
//! | [`CapabilityIndex::capability_fields_for`]    | 同上 `capabilityFieldsFor` / `regionFields`       |
//! | 静态兜底表                                     | `internal/server/handler.go` `staticModels` / `staticModelsIntl` |
//! | （第二批）[`CapabilityIndex::merged_model_list`] | 同上 `mergedModelList`                          |
//!
//! # 为什么单独成文件、且**不含**任何 IO
//!
//! 本模块只做**纯数据/合并逻辑**：给定「已经拉到的动态清单」与「静态兜底表」，
//! 算出能力索引与合并目录。它**不发** HTTP 请求、**不读** [`crate::server::AppState`]、
//! **不碰**账号池 —— 「该区域有没有可用账号」由调用方以 `has_account` 传入。
//!
//! 这样拆的两个理由：
//!
//! 1. 能力索引的正确性全是**语义**问题（三态、静态表不该否定存在、两区谁赢），
//!    与网络/缓存无关，混在一起会让这些语义淹没在重试逻辑里；
//! 2. 它可以被独立单测 —— Go 侧这些函数挂在 `*Handler` 上，测一条语义要造整个
//!    Handler（含池子、配置、动态快照），于是**没人测**，缺陷就藏在里面。
//!
//! # 与 Go 的刻意差异
//!
//! - Go 的 `buildCapabilityIndex` 自己调 `h.fetchModelsForRegion`（带缓存与指数退避）；
//!   Rust 侧缓存策略在调用方，本模块只收结果。`modelsRetryAfter`（15s→…→5min）、
//!   `dynamicModelsTTL`（1h）、`regionModelCache` 都**不在**本模块。
//! - Go 的 `regionGapReason` 查池子（`probeAccountInRegion`）；Rust 侧收 `has_account`。
//! - Go 的 `productModels` / `productRegions` 读 `DynamicSnapshot()` / `cfg.Pool`；
//!   Rust 侧作入参（见第二批的 `merged_model_list`）。
//!
//! # 三态语义（本模块最容易改错的地方）
//!
//! 上游对「有没有图片能力」「有没有倍率」都是**可以什么都不写**的，因此两者都是
//! **三态**而非布尔：
//!
//! ```text
//! None       = 上游未声明 → 「不知道」，**不能**当成 false / 免费
//! Some(false)= 上游明确否定
//! Some(true) = 上游明确肯定
//! ```
//!
//! 把它当成布尔的两个方向都是**真实发生过的缺陷**：谎报 `false` 会让客户端把本可用的
//! 图片/思考能力关掉；把未声明的倍率当 0 会让该计费的模型统计成免费。

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

use crate::auth::Region;
use crate::resolve_model::{realm_to_region, resolve_model, PRODUCT_WORKBUDDY};
use crate::upstream::{measured_override, ModelInfo};

/// 国服区域码。★ 与 [`Region::as_str`] **同值**（Go `:478-481` 注释逐字：
/// 「与 `auth.Region.String()` 同值，下发字段与 auth 包必须用同一套拼写，
/// 否则前端要认两套码」）。
pub const REGION_CODE_CN: &str = "cn";

/// 国际版区域码。同 [`REGION_CODE_CN`] 的理由。
pub const REGION_CODE_INTL: &str = "intl";

/// 静态表条目的 `created`（与 Go 两张表逐字一致）。
const STATIC_CREATED: i64 = 1753600000;

/// 一个模型名在某区域的能力真值。
///
/// 九个字段全部对应 Go `regionCapability`（`:355-418`），语义逐个照抄：
///
/// # `present`：区分「列了但不支持」与「压根没列」
///
/// 两者的**用户提示完全不同**：前者该换模型，后者该换账号/换区域。
/// 合成一个「可用/不可用」会让提示指向错误的行动。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RegionCapability {
    /// 该区域的上游清单里**列了这个名字**。
    pub present: bool,

    /// 图片能力的**有效**值（= 上游声明经实测覆盖之后）。
    ///
    /// `None` = 不知道（上游未声明且未实测）⇒ **不下发**任何能力字段，
    /// 而不是下发 `false`（见 [`model_capability_fields`]）。
    pub supports_images: Option<bool>,

    /// 上游**原话**（未经实测覆盖），仅用于诊断对比。
    ///
    /// 留着它的理由：实测发现国际版 glm-5.x 上游报 `true` 而实测读不到图
    ///（图片被替换成固定占位符），此时 `supports_images` 是 `false` 而本字段
    /// 仍是 `true` —— 排查「上游到底怎么说的」不必再手工拉一次。
    pub upstream_images: Option<bool>,

    /// 本条来自**手抄静态兜底表**。
    ///
    /// ★ 判断「另一区域没有这个模型」时**必须排除**静态来源：静态表会过期，
    /// 用于「该区域有哪些模型」可信，用于「另一区域**没有**某模型」不可信。
    /// 实测踩过：国服静态表漏了 glm-5.3，于是把带图片的 glm-5.3 判成
    /// 「只能去国际版」—— 正好推向读不到图的那一侧。
    pub from_static: bool,

    /// 上游声明的可选思考档位（空 = 未声明 / 只有默认档）。
    pub efforts: Vec<String>,

    /// 上游给的**默认**思考档（空 = 未声明）。
    pub default_effort: String,

    /// 上游声明的「该模型是否有思考能力」。
    ///
    /// ★ `None` = **未声明**，**不是** `false`。早先只按 [`Self::efforts`] 是否为空
    /// 判断，把「只有固定档、没有档位数组」的模型误判成不支持思考 ——
    /// 18 个（国服）/ 8 个（国际版）模型因此在界面上显示成「—」。
    pub supports_reasoning: Option<bool>,

    /// 是否允许关闭思考（`reasoning.canDisableThinking`）。
    ///
    /// 为 `Some(false)` 时客户端**不能传 `off`**，应据此置灰。
    pub can_disable_thinking: Option<bool>,

    /// 该模型的**计费倍率**（上游 `/v3/config` 的 `credits`）。
    ///
    /// ★ 按**区域**存而非按模型名存：同名模型在两区可能是不同后端且计费不同。
    /// 实测 2026-09-18 拉两区 `/v3/config`：
    ///
    /// ```text
    /// deepseek-v4.1-flash   国服 "x0.03"（计费）  国际版 "x0.00"（免费）
    /// ```
    ///
    /// 三态：`None` = 未声明（**不是**免费）。上游只给部分模型写该字段
    ///（实测国服 52 个里只有 33 个），把未声明当免费会把该计费的统计成 0。
    pub credit_multiplier: Option<f64>,
}

/// 区域缺口的原因。**两种原因的行动建议完全相反**，故必须是枚举而非文案。
///
/// ★ 绝不要靠解析 `reason` 字符串里的关键词来判断种类 —— 文案改一个字就失效，
/// 而失效的表现是**给出错误的行动建议**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapCause {
    /// 该区域**一个可用账号都没有** ⇒ 「补账号」是对的。
    MissingAccounts,
    /// 有账号但清单本次没拉到 ⇒ 「补账号」毫无用处，该等自动重试。
    FetchFailed,
}

/// 某区域的缺口说明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionGap {
    /// 原因枚举（判定**只**看它）。
    pub cause: GapCause,
    /// 给人看的文案（含区域名与行动建议）。
    pub reason: String,
}

/// 能力索引：`区域 → 模型名 → 能力真值`，外加「哪些区域有真值」与缺口原因。
///
/// 对应 Go `capabilityIndex`（`:421-432`）。
#[derive(Debug, Clone, Default)]
pub struct CapabilityIndex {
    /// 按区域分桶的能力真值。**只**含 [`Region::CN`] / [`Region::Intl`]。
    by_region: HashMap<Region, HashMap<String, RegionCapability>>,

    /// 该区域**有动态真值**（拉到了非空清单）。
    ///
    /// ★ 只对动态真值置位：静态表用于「该区域有哪些模型」可信，
    /// 用于「另一区域没有某模型」**不可信**（表会过期）。
    known_region: HashMap<Region, bool>,

    /// 该区域**没有**动态真值时的缺口说明。只在 `known_region == false` 时存在。
    gap_reason: HashMap<Region, RegionGap>,
}

impl CapabilityIndex {
    /// 空索引（任何 `lookup` 都返回 `None`）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 用「已拉到的动态清单」+「静态兜底表」建索引。
    ///
    /// `fetched` 里每个区域给 `Option<Vec<ModelInfo>>`：
    ///
    /// - `Some(非空)` = **动态真值** ⇒ `known_region = true`，**不**回落静态表；
    /// - `None` / `Some(空)` = 没拉到 ⇒ 该区域回落静态表并记 `gap_reason`。
    ///
    /// `has_account` 决定缺口的 [`GapCause`]：该区域**有没有可用账号**。
    ///
    /// # 为什么静态表也要入表
    ///
    /// 不入的话，`/v1/models` 在回退期间对**整批**模型都不下发能力字段，
    /// 客户端全部按纯文本处理 —— 图片能力整个消失，而网关看起来完全正常
    ///（这正是最难排查的一类问题）。
    pub fn build(
        fetched: &HashMap<Region, Option<Vec<ModelInfo>>>,
        has_account: &HashMap<Region, bool>,
    ) -> Self {
        let mut idx = CapabilityIndex::new();

        // 两个区域互相独立，顺序不影响结果（Go 侧遍历 map，顺序随机）。
        for region in [Region::CN, Region::Intl] {
            let dynamic: Option<&Vec<ModelInfo>> = fetched
                .get(&region)
                .and_then(|v| v.as_ref())
                .filter(|v| !v.is_empty());

            if let Some(infos) = dynamic {
                idx.add(region, infos, true);
                idx.known_region.insert(region, true);
                continue;
            }

            let has = has_account.get(&region).copied().unwrap_or(false);
            idx.gap_reason.insert(region, region_gap_reason(region, has));
            let fallback = infos_from_static(region);
            idx.add(region, &fallback, false);
        }

        idx
    }

    /// 把一批模型信息写进某区域。
    ///
    /// `dynamic = false` 表示这批来自静态兜底表 ⇒ `from_static = true`。
    fn add(&mut self, region: Region, infos: &[ModelInfo], dynamic: bool) {
        let m = self.by_region.entry(region).or_default();
        for mi in infos {
            // 空 id 直接跳过：它会变成一个「名字为空」的条目，
            // 在 /v1/models 里表现为一条谁也调不到的幽灵模型。
            if mi.id.is_empty() {
                continue;
            }
            m.insert(
                mi.id.clone(),
                RegionCapability {
                    present: true,
                    // ★ 实测覆盖：国际版 glm-5.x 上游报 true、实测读不到图，
                    // 必须以实测为准（否则客户端会发出必然静默降级的请求）。
                    supports_images: measured_override(&mi.id, region, mi.supports_images),
                    upstream_images: mi.supports_images,
                    from_static: !dynamic,
                    efforts: mi.efforts.clone(),
                    default_effort: mi.default_effort.clone(),
                    supports_reasoning: mi.supports_reasoning,
                    can_disable_thinking: mi.can_disable_thinking,
                    credit_multiplier: mi.credit_multiplier,
                },
            );
        }
    }

    /// 查某模型在某区域的真值。区域无桶或该名字未收录 ⇒ `None`。
    pub fn lookup(&self, id: &str, region: Region) -> Option<&RegionCapability> {
        self.by_region.get(&region)?.get(id)
    }

    /// `fillBilling` 用的倍率。
    ///
    /// `region` 无该模型 / 未声明 ⇒ `None`（**绝不编造**，也**绝不**回落成 0 ——
    /// 未声明与免费是两回事，见 [`RegionCapability::credit_multiplier`]）。
    pub fn multiplier(&self, region: Region, model: &str) -> Option<f64> {
        self.lookup(model, region).and_then(|c| c.credit_multiplier)
    }

    /// 该区域是否**有动态真值**（静态兜底不算）。
    pub fn is_known_region(&self, region: Region) -> bool {
        self.known_region.get(&region).copied().unwrap_or(false)
    }

    /// 该区域的缺口说明（只在没有动态真值时存在）。
    pub fn gap_reason_for(&self, region: Region) -> Option<&RegionGap> {
        self.gap_reason.get(&region)
    }

    /// 生成某模型名应下发的**能力字段**（`/v1/models` 用）。
    ///
    /// 对应 Go `capabilityFieldsFor`（`:1355-1398`）。三类情形：
    ///
    /// - 两区都有 ⇒ 用**国服**真值（国服清单权威，且与「带图片的请求偏好国服」
    ///   的运行时策略一致）；
    /// - 只一区有 ⇒ 用该区真值，并附 `supported_regions` 说明；
    /// - 未收录 ⇒ 空 map（三态中的「未声明」）。
    ///
    /// ★ 关键：**只在一区存在并不等于不支持图片**。网关会按区域路由把该模型的
    /// 请求送到它所在的区域，因此必须如实下发该区域的能力，而不是降级为 `false`
    /// —— 降级会让客户端隐藏图片入口。
    ///
    /// ★ 图片与思考是**正交**的两个维度：图片能力为 `None` 时思考字段仍要下发
    ///（不能因为 `fields` 为空就整个跳过）。
    pub fn capability_fields_for(&self, id: &str) -> Map<String, Value> {
        let cap = match (self.lookup(id, Region::CN), self.lookup(id, Region::Intl)) {
            (Some(cn), _) if cn.present => cn,
            (_, Some(intl)) if intl.present => intl,
            _ => return Map::new(),
        };

        let image = model_capability_fields(cap.supports_images);
        let reasoning = model_reasoning_fields(&cap.efforts, &cap.default_effort);
        if image.is_none() && reasoning.is_none() {
            return Map::new();
        }

        let mut out = image.unwrap_or_default();
        if let Some(r) = reasoning {
            for (k, v) in r {
                out.insert(k, v);
            }
        }

        // 为 false 时客户端**不能传 off**，故两种拼写都下发。
        if let Some(v) = cap.can_disable_thinking {
            out.insert("can_disable_thinking".into(), json!(v));
            out.insert("canDisableThinking".into(), json!(v));
        }

        if let Some(rf) = self.region_fields(id) {
            for (k, v) in rf {
                out.insert(k, v);
            }
        }

        out
    }

    /// 区域维度的「已知/未知」字段（`supported_regions` / `unverified_regions` /
    /// `region_note`）。对应 Go `regionFields`（`:1415-1461`）。
    ///
    /// 只在**恰好一个区域有动态真值且收录了该模型**时才下发：
    ///
    /// - 两区都有 ⇒ 无需说明（返回 `None`）；
    /// - 一区都没有 ⇒ 也无需说明（`capability_fields_for` 早已返回空）；
    /// - **只有静态表里有** ⇒ 不算「支持」⇒ 返回 `None`。
    fn region_fields(&self, id: &str) -> Option<Map<String, Value>> {
        let mut supported: Vec<String> = Vec::new();
        for (region, code) in [
            (Region::CN, REGION_CODE_CN),
            (Region::Intl, REGION_CODE_INTL),
        ] {
            if let Some(c) = self.lookup(id, region) {
                // ★ `!from_static` 是必需的：静态表「只提供信息，不否定存在」，
                // 拿它去断言「另一区没有」会造出错误的行动建议。
                if c.present && !c.from_static {
                    supported.push(code.to_string());
                }
            }
        }
        if supported.len() != 1 {
            return None;
        }

        let region = supported[0].clone();
        let other = other_region_code(&region);
        let mut m = Map::new();
        m.insert("supported_regions".into(), json!([region.clone()]));

        if !self.is_known_region(region_by_code(other)) {
            let gap = self
                .gap_reason_for(region_by_code(other))
                .filter(|g| !g.reason.is_empty())
                .cloned()
                .unwrap_or_else(|| RegionGap {
                    // ★ 兜底必须归为 **FetchFailed**：不知道原因时说「补账号」
                    // 会给出错误的行动建议（所有者明明有账号却被要求补账号）。
                    cause: GapCause::FetchFailed,
                    reason: format!("{}的模型清单本次未拉到", region_label(other)),
                });
            m.insert("unverified_regions".into(), json!([other]));
            m.insert(
                "region_note".into(),
                json!(unverified_note(&region, other, &gap)),
            );
            return Some(m);
        }

        m.insert("region_note".into(), json!(region_note(&region)));
        Some(m)
    }
}

// ---------------------------------------------------------------------------
// 静态兜底表（`handler.go` `staticModels` / `staticModelsIntl`）
// ---------------------------------------------------------------------------

/// 静态表里的一条（只存三样：`object` / `created` 两张表都相同，故不入表）。
struct StaticModel {
    /// 模型 ID。
    id: &'static str,
    /// `owned_by`：国服表是 `workbuddy`，国际版表是 `workbuddy-intl`。
    owned_by: &'static str,
    /// 上下文长度（客户端据此决定何时压缩历史）。
    context_length: i64,
}

/// 国服静态表（Go `staticModels`，实测 2026-09-16 共 16 个）。
///
/// ★ 过期会**真的出错**，不只是展示问题：能力真值表用这张表兜底，若它缺了
/// `glm-5.3`，网关就会认为「国服没有 glm-5.3」，于是把带图片的 glm-5.3 请求
/// 判成「只能去国际版」—— 正好与事实相反（实测只有国服能读图）。
/// 也就是说，静态表错误会把图片请求推向读不到图的那一侧。
const STATIC_MODELS_CN: &[StaticModel] = &[
    StaticModel { id: "auto", owned_by: "workbuddy", context_length: 256000 },
    StaticModel { id: "hy4-preview", owned_by: "workbuddy", context_length: 1000000 },
    StaticModel { id: "hy3", owned_by: "workbuddy", context_length: 192000 },
    StaticModel { id: "hy3-x", owned_by: "workbuddy", context_length: 192000 },
    StaticModel { id: "deepseek-v4.1-flash", owned_by: "workbuddy", context_length: 1000000 },
    StaticModel { id: "deepseek-v4-pro", owned_by: "workbuddy", context_length: 1000000 },
    StaticModel { id: "glm-5.3", owned_by: "workbuddy", context_length: 1000000 },
    StaticModel { id: "glm-5.3-flash", owned_by: "workbuddy", context_length: 1000000 },
    StaticModel { id: "glm-5.2", owned_by: "workbuddy", context_length: 1000000 },
    StaticModel { id: "glm-5.1", owned_by: "workbuddy", context_length: 200000 },
    StaticModel { id: "glm-5v-turbo", owned_by: "workbuddy", context_length: 200000 },
    StaticModel { id: "minimax-m3", owned_by: "workbuddy", context_length: 512000 },
    StaticModel { id: "kimi-k3-1", owned_by: "workbuddy", context_length: 1000000 },
    StaticModel { id: "kimi-k2.8-preview", owned_by: "workbuddy", context_length: 1000000 },
    StaticModel { id: "kimi-k2.7", owned_by: "workbuddy", context_length: 256000 },
    StaticModel { id: "kimi-k2.6", owned_by: "workbuddy", context_length: 256000 },
];

/// 国际版静态表（Go `staticModelsIntl`，实测 2026-09-15 共 20 个）。
///
/// # ⚠ `hy4-preview` **不在**本表（2026-09-21 所有者要求）
///
/// 所有者原话：
///
/// ```text
/// 「国际版既然不支持，为什么你还能跑出来这个模型？接口返回没有就不要
///   搞出来，懂吗？」
/// ```
///
/// 他的要求是：**上游没返回的模型，一个都不许出现在清单里**。旧代码把
/// `hy4-preview` 同时写进两张表，于是 `/v1/models` 把它标成「国服 + 国际版」
/// 双渠道，但国际版根本调不到它 —— 用户按清单指定国际版就会失败。
///
/// 实测（2026-09-21，用他自己的 5 个国际版账号拉两区真值）：
///
/// ```text
/// 国服   16 个模型  含 hy4-preview     ✓
/// 国际版 22 个模型 **不含 hy4-preview** ✗
/// 国际版有 hy4-preview-f（注意 -f 后缀，是**另一个**模型）
/// ```
///
/// 国服那张表**保留**它：国服真值确实有（且实测出流正常）。
///
/// 注意与国服的差异（客户端选模型时最易踩的坑）：
///
/// ```text
/// 国服   deepseek-v4-flash   / glm-5.2 / kimi-k2.7 / minimax-m3
/// 国际版 deepseek-v4.1-flash / glm-5.3 / kimi-k3   / gpt-5.6-* / gemini-3.5-flash
/// ```
const STATIC_MODELS_INTL: &[StaticModel] = &[
    StaticModel { id: "default-model", owned_by: "workbuddy-intl", context_length: 200000 },
    StaticModel { id: "fast-model", owned_by: "workbuddy-intl", context_length: 200000 },
    StaticModel { id: "balanced-model", owned_by: "workbuddy-intl", context_length: 256000 },
    StaticModel { id: "primary-model", owned_by: "workbuddy-intl", context_length: 272000 },
    StaticModel { id: "deep-model", owned_by: "workbuddy-intl", context_length: 200000 },
    // ⚠ hy4-preview-f **保留**（国际版真值里有它），hy4-preview 已移除（见上方注释）。
    StaticModel { id: "hy4-preview-f", owned_by: "workbuddy-intl", context_length: 300000 },
    StaticModel { id: "hy3", owned_by: "workbuddy-intl", context_length: 192000 },
    StaticModel { id: "deepseek-v4.1-flash", owned_by: "workbuddy-intl", context_length: 300000 },
    StaticModel { id: "gpt-6-astra", owned_by: "workbuddy-intl", context_length: 400000 },
    StaticModel { id: "gpt-5.6-sol", owned_by: "workbuddy-intl", context_length: 1000000 },
    StaticModel { id: "gpt-5.6-terra", owned_by: "workbuddy-intl", context_length: 1000000 },
    StaticModel { id: "gpt-5.6-luna", owned_by: "workbuddy-intl", context_length: 1000000 },
    StaticModel { id: "gpt-5.5", owned_by: "workbuddy-intl", context_length: 1000000 },
    StaticModel { id: "gpt-5.4", owned_by: "workbuddy-intl", context_length: 272000 },
    StaticModel { id: "gemini-3.5-flash", owned_by: "workbuddy-intl", context_length: 1000000 },
    StaticModel { id: "glm-5.3", owned_by: "workbuddy-intl", context_length: 1000000 },
    StaticModel { id: "glm-5.2", owned_by: "workbuddy-intl", context_length: 1000000 },
    StaticModel { id: "kimi-k3", owned_by: "workbuddy-intl", context_length: 1000000 },
    StaticModel { id: "kimi-k2.8-preview", owned_by: "workbuddy-intl", context_length: 300000 },
    StaticModel { id: "kimi-k2.6", owned_by: "workbuddy-intl", context_length: 256000 },
];

/// 取某区域的静态表（[`Region::Any`] ⇒ 空）。
fn static_table(region: Region) -> &'static [StaticModel] {
    match region {
        Region::CN => STATIC_MODELS_CN,
        Region::Intl => STATIC_MODELS_INTL,
        Region::Any => &[],
    }
}

/// 静态表里全部模型 ID（保持表内顺序）。
pub fn static_model_ids(region: Region) -> Vec<String> {
    static_table(region).iter().map(|m| m.id.to_string()).collect()
}

/// 静态表的完整条目（含 `object` / `created` / `owned_by` / `context_length`
/// 与 `withImageCapability` 补上的能力字段）。
///
/// 供 `/v1/models` 的静态兜底与 `base` 元数据使用。
pub fn static_entries(region: Region) -> Vec<Value> {
    static_table(region).iter().map(static_entry).collect()
}

/// 单条静态表条目 → 下发形状。
///
/// 对应 Go `withImageCapability`（`handler.go:634-644`）：给每条补上
/// `modelCapabilityFields(&true)` 的键，**但不覆盖已存在的键**。
///
/// 静态表取自 `/v3/config` 的 `agents[cli].models`，实测该清单下**全部**模型
/// `supportsImages = true`，因此统一标注为支持图片。不标注的话，回退期间客户端
/// 会把所有模型当纯文本 —— 图片能力整个消失。
fn static_entry(sm: &StaticModel) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), json!(sm.id));
    m.insert("object".into(), json!("model"));
    m.insert("created".into(), json!(STATIC_CREATED));
    m.insert("owned_by".into(), json!(sm.owned_by));
    m.insert("context_length".into(), json!(sm.context_length));

    if let Some(caps) = model_capability_fields(Some(true)) {
        for (k, v) in caps {
            if !m.contains_key(&k) {
                m.insert(k, v);
            }
        }
    }

    Value::Object(m)
}

/// 静态表条目 → [`ModelInfo`]（供 `build` 走静态兜底路径）。
///
/// ★ 只带 **ID + `supports_images = true`**，其余字段保持零值 ——
/// 静态表**不编造**档位、默认档、倍率（编造的档位会被客户端拿去发请求，
/// 表现为「我明明调了 max 却没生效」这类无从排查的现象）。
fn infos_from_static(region: Region) -> Vec<ModelInfo> {
    static_table(region)
        .iter()
        .map(|sm| ModelInfo {
            id: sm.id.to_string(),
            supports_images: Some(true),
            ..Default::default()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 区域码 / 文案
// ---------------------------------------------------------------------------

/// 区域码 → [`Region`]。未知码按**国服**（保守），与 Go `regionByCode` 一致。
fn region_by_code(code: &str) -> Region {
    if code == REGION_CODE_INTL {
        Region::Intl
    } else {
        Region::CN
    }
}

/// 另一个区域的区域码。
fn other_region_code(region: &str) -> &'static str {
    if region == REGION_CODE_INTL {
        REGION_CODE_CN
    } else {
        REGION_CODE_INTL
    }
}

/// 区域码 → 中文区域名（入参是**区域码**，不是模型名里的前缀）。
fn region_label(region: &str) -> &'static str {
    if region == REGION_CODE_INTL {
        "国际版"
    } else {
        "国服"
    }
}

/// 该区域缺口的原因。
///
/// `has_account` = 该区域**有没有可用账号**（调用方查池子得出）。
fn region_gap_reason(region: Region, has_account: bool) -> RegionGap {
    if !has_account {
        RegionGap {
            cause: GapCause::MissingAccounts,
            reason: format!("账号池里没有{}的可用账号", region_label(region.as_str())),
        }
    } else {
        RegionGap {
            cause: GapCause::FetchFailed,
            reason: format!(
                "{}账号清单本次未拉到（上游暂时不可达，会自动重试）",
                region_label(region.as_str())
            ),
        }
    }
}

/// 「该模型名只在某区上游存在」的说明。
fn region_note(region: &str) -> String {
    let other = region_label(other_region_code(region));
    format!(
        "该模型名仅在{}上游存在；账号池里的{}账号调用它会返回 11102 model service info not found。",
        region_label(region),
        other
    )
}

/// 「另一区**未检测到可用真值**」的说明。
///
/// ★ 两种 cause 的结尾**必须不同**：`MissingAccounts` 建议补账号，
/// `FetchFailed` 说明这是上游暂时不可达、**正在自动重试**。
///
/// 所有者 2026-09-20 原话（针对旧文案让他手动同步）：
///
/// ```text
/// 「这应该是自动的,而不是需要人手动同步,你懂吗?」
/// ```
///
/// 因此 `FetchFailed` 分支的措辞是「**正在自动重试**，无需手动刷新」，
/// 绝不写成「请点刷新」。
fn unverified_note(region: &str, other: &str, gap: &RegionGap) -> String {
    let head = format!(
        "已确认{}上游有此模型；但{}未检测到可用真值（{}），因此**无法确认**{}是否也有它 —— 这不等于该模型{}没有。",
        region_label(region),
        region_label(other),
        gap.reason,
        region_label(other),
        region_label(other)
    );
    match gap.cause {
        GapCause::MissingAccounts => {
            format!("{}补齐{}账号后即可确认。", head, region_label(other))
        }
        GapCause::FetchFailed => format!(
            "{}你的{}账号是够的，这是上游暂时不可达；**正在自动重试**，无需手动刷新。",
            head,
            region_label(other)
        ),
    }
}

// ---------------------------------------------------------------------------
// 能力字段生成
// ---------------------------------------------------------------------------

/// 图片能力字段。
///
/// `None`（上游未声明）⇒ **不下发**任何字段：宁可不写，也不要谎报成纯文本 ——
/// 后者会让本可用的图片能力被客户端主动关掉。
///
/// 一次下发**多种拼写**的理由：各客户端读的字段名不统一，且都只在各自的 provider
/// 专用解析器里读，没有统一约定（实测 2026-09-16，见各客户端源码）：
///
/// ```text
/// OpenClaw OpenAI Codex   → input_modalities / inputModalities
/// OpenClaw Copilot        → capabilities.supports.vision
/// OpenClaw HuggingFace    → architecture.input_modalities
/// OpenClaw OpenRouter     → architecture.modality（"text+image->text"）
/// OpenClaw Vercel AI GW   → tags 含 "vision"
/// OpenClaw LM Studio      → capabilities.vision
/// ZCode / DSH /v1/models  → 只读 id / context，不读能力字段
/// ```
///
/// 多写几种是安全的：已知解析器都只取自己认识的键，多余键不会报错。
fn model_capability_fields(supports_images: Option<bool>) -> Option<Map<String, Value>> {
    let yes = match supports_images {
        None => return None,
        Some(v) => v,
    };

    let mut m = Map::new();
    if !yes {
        // 显式不支持：明确告知，避免客户端按「默认支持」处理。
        m.insert("supportsImages".into(), json!(false));
        m.insert(
            "capabilities".into(),
            json!({ "vision": false, "supports": { "vision": false } }),
        );
        return Some(m);
    }

    m.insert("supportsImages".into(), json!(true));
    m.insert("input_modalities".into(), json!(["text", "image"]));
    m.insert("inputModalities".into(), json!(["text", "image"]));
    m.insert(
        "capabilities".into(),
        json!({ "vision": true, "supports": { "vision": true } }),
    );
    m.insert(
        "architecture".into(),
        json!({ "input_modalities": ["text", "image"], "modality": "text+image->text" }),
    );
    m.insert("tags".into(), json!(["vision"]));
    m.insert(
        "modalities".into(),
        json!({ "input": ["text", "image"], "output": ["text"] }),
    );
    Some(m)
}

/// 思考档位字段。对应 Go `modelReasoningFields`（`:1652-1681`）。
///
/// **三种情形必须分开表达**（2026-09-18 实测修正，所有者报的就是这个缺陷）：
///
/// ```text
/// A. efforts 非空        → 如实列出上游声明的那组档位（+ 默认档），不做任何推断
/// B. efforts 空但有默认档 → **未声明可选范围**：标准阶梯全部可用 + 标记
/// C. 两者都空            → 上游没声明任何思考信息，不下发任何键
/// ```
///
/// ★ 早先只判 `efforts.is_empty() → None`，把 B 与 C 混成一种，于是
/// 18 个（国服）/ 8 个（国际版）模型在界面上显示成「—」，用户以为它们不能思考。
///
/// ★ 情形 B **绝不能**标成「固定单档」（`reasoning_fixed = true`）—— 那是**错的**。
/// 实测 `deepseek-v4.1-flash`（未声明 `supportedEfforts` 的典型）：
///
/// ```text
/// reasoning_effort=off         → ✗ HTTP 400 the reasoning effort value is not supported
/// reasoning_effort=bogus_value → ✗ HTTP 400 同上
/// minimal/low/medium/high/max/xhigh → ✓ 全部接受
///
/// 单调性（各档 2 次，推理字符均值）：
///     low 397 → medium 420 → high 646 → max 776   ✓ 单调递增
/// ```
///
/// 随机性不会产生单调序列，所以**档位真的生效**（所有者原话：
/// 「我现在就用的这个模型，用的 max 档位，为什么没有拦截报错？」）。
/// 上游拒绝 `off` / 未知值，说明它有一整套**标准档位阶梯**在校验，
/// 只是没在 `/v3/config` 里逐模型列出。
///
/// ★ 情形 A 的「声明了」**不等于**「只有这些可用」：实测 `hy3` 声明 `[low,high]`
/// 却接受 `medium/max/minimal/off`。所以这组值只是「上游愿意列出来的」，
/// 不是硬范围 —— 这也是**不能**据此拦截请求的原因（见 `forward` 的区域路由）。
fn model_reasoning_fields(efforts: &[String], default_effort: &str) -> Option<Map<String, Value>> {
    let default_effort = default_effort.trim();

    // 情形 C。
    if efforts.is_empty() && default_effort.is_empty() {
        return None;
    }

    // 情形 A：上游声明了该模型的可选档位 → 如实列出它自己的范围。
    if !efforts.is_empty() {
        return Some(with_effort_list(efforts, default_effort, false));
    }

    // 情形 B：只有默认档 ⇒ 未声明可选范围，给出标准阶梯并**明确标记**。
    Some(with_effort_list(&standard_efforts(), default_effort, true))
}

/// 组装档位字段（多拼写）。
///
/// ★ 必须**复制** `list`：它可能是调用方持有的共享切片，就地改动会污染来源
///（Go 侧注释逐字：「list 可能是被缓存的共享切片」）。
fn with_effort_list(
    list: &[String],
    default_effort: &str,
    undeclared_range: bool,
) -> Map<String, Value> {
    let cp: Vec<String> = list.to_vec();
    let cp_value = json!(cp);

    let mut nested = Map::new();
    nested.insert("supported_efforts".into(), cp_value.clone());

    let mut out = Map::new();
    // OpenAI 风格 / 通用容错拼写。
    out.insert("supported_efforts".into(), cp_value.clone());
    out.insert("supportedEfforts".into(), cp_value.clone());
    out.insert("reasoning_efforts".into(), cp_value.clone());
    out.insert("reasoningEfforts".into(), cp_value);
    out.insert("supports_reasoning".into(), json!(true));
    out.insert("supportsReasoning".into(), json!(true));
    out.insert("reasoning_fixed".into(), json!(false));
    out.insert("reasoningFixed".into(), json!(false));

    if undeclared_range {
        nested.insert("range_undeclared".into(), json!(true));
        out.insert("reasoning_range_undeclared".into(), json!(true));
        out.insert("reasoningRangeUndeclared".into(), json!(true));

        // 逐档标注「上游是否**显式声明**过它」：标准阶梯里有几档是网关推出来的，
        // 客户端若想保守可以只看 `true` 的那些。
        let mut backed = Map::new();
        for e in &cp {
            backed.insert(e.clone(), json!(upstream_declared_effort(e)));
        }
        let backed = Value::Object(backed);
        nested.insert("declared_by_upstream".into(), backed.clone());
        out.insert("upstream_declared_efforts".into(), backed);
    }

    // 默认档只在非空时下发。**不要**用 `efforts[0]` 之类的猜测填充：
    // 上游没声明默认档时，网关也不知道，编一个反而误导。
    if !default_effort.is_empty() {
        nested.insert("default_effort".into(), json!(default_effort));
        out.insert("default_effort".into(), json!(default_effort));
        out.insert("defaultEffort".into(), json!(default_effort));
        out.insert("default_reasoning_effort".into(), json!(default_effort));
    }

    out.insert("reasoning".into(), Value::Object(nested));
    out
}

// ---------------------------------------------------------------------------
// 标准档位阶梯（Go `internal/upstream/payload.go`）
// ---------------------------------------------------------------------------

/// 档位 → 强度排名（Go `effortRank`，`payload.go:116`）。
///
/// ★ 这张表在 Rust `upstream/payload.rs:24` 有一份**私有**副本
///（`fn effort_rank`），但未导出，而 `upstream/**` 不在本任务的写范围内，
/// 故这里本地复制一份。两处若漂移，表现为「网关认为的标准阶梯与改写请求体时
/// 认可的顺序不一致」—— 本模块只用于**下发**（不校验、不拦截），
/// 因此漂移不会造成拒绝请求，但会让清单里的顺序失去意义。
const EFFORT_RANK: &[(&str, i32)] = &[
    ("off", 0),
    ("minimal", 1),
    ("low", 2),
    ("medium", 3),
    ("high", 4),
    ("xhigh", 5),
    ("max", 6),
];

/// 上游**显式声明过**的档位（Go `upstreamDeclaredEfforts`，`payload.go:126`）。
///
/// 用于情形 B 的 `declared_by_upstream` 标记：`off` / `minimal` 不在其中，
/// 因为实测没有任何模型声明过它们。
const UPSTREAM_DECLARED_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// 标准档位阶梯，按强度**升序**（Go `upstream.StandardEfforts()`，`payload.go:145`）。
fn standard_efforts() -> Vec<String> {
    let mut v: Vec<(&str, i32)> = EFFORT_RANK.to_vec();
    v.sort_by_key(|(_, rank)| *rank);
    v.into_iter().map(|(name, _)| name.to_string()).collect()
}

/// 某档位是否被上游显式声明过（Go `upstream.UpstreamDeclaredEffort`）。
fn upstream_declared_effort(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    UPSTREAM_DECLARED_EFFORTS.contains(&n.as_str())
}

// ---------------------------------------------------------------------------
// 渠道（Go `productChannel` / `appendChannel` / `channelsToJSON`）
// ---------------------------------------------------------------------------

/// 渠道标签（Go `channelLabels`，`:724-730`）。
///
/// 只用于**展示**：界面上的平台名。认不出的产品回落成产品标识本身
///（见 `merged_model_list` 里 `channelLabels[pm.Product] == ""` 的分支）——
/// 宁可显示一个英文标识，也不显示空白。
fn channel_label(product: &str) -> Option<&'static str> {
    match product {
        "workbuddy" => Some("WorkBuddy"),
        "qoder" => Some("Qoder"),
        "zcode" => Some("ZCode"),
        "trae" => Some("Trae"),
        "doubao" => Some("豆包"),
        _ => None,
    }
}

/// 一个模型的一条「来自哪个平台/区域」的渠道（Go `productChannel`，`:692-718`）。
///
/// `region` 为空串表示**该平台不分区**（Qoder / ZCode 的账号池不分 cn/intl，
/// 故一个产品只留一条渠道）。
///
/// # `has_multiplier` 与 `credit_multiplier` 是**两个**问题
///
/// - `has_multiplier`：这个平台**有没有倍率这个概念**（WorkBuddy 有，ZCode 没有）；
/// - `credit_multiplier`：这一条**取到值了吗**。
///
/// 分开的必要性：WorkBuddy 静态兜底期间倍率恒为 `None`（手抄表里没有 credits），
/// 但界面应显示「—」（不知道）而**不是**整列消失 —— 后者会让人以为这个平台
/// 不计费。而 ZCode 是真的没有这个概念，显示空列才是对的。
#[derive(Debug, Clone, PartialEq)]
pub struct ProductChannel {
    /// 产品标识（`workbuddy` / `qoder` / …）。
    pub product: String,
    /// 展示名（[`channel_label`]）。
    pub label: String,
    /// 区域码（[`REGION_CODE_CN`] / [`REGION_CODE_INTL`]）；空串 = 不分区。
    pub region: String,
    /// 该渠道的计费倍率；`None` = 未声明。
    pub credit_multiplier: Option<f64>,
    /// 该平台是否有倍率概念。
    pub has_multiplier: bool,
}

/// 追加一条渠道，去重键是 **(product, region)**（Go `appendChannel`，`:1203-1219`）。
///
/// # 为什么去重键必须含 region
///
/// 计费倍率**按区域不同**。实测同名模型在两区是不同的后端：
///
/// ```text
/// deepseek-v4.1-flash   国服 "x0.03"（计费）   国际版 "x0.00"（免费）
/// ```
///
/// 旧实现按 `product` 合并区域（一条 `WorkBuddy` 带 `regions:["cn","intl"]`），
/// 那样一条渠道只能承载一个倍率，两区不同时**必然有一个显示错**。
/// 而「显示错的倍率」比「不显示」更糟 —— 用户会据它判断该烧哪个账号的额度。
///
/// # 命中已存在时：**已声明优先**
///
/// `if 旧的 is_none() && 新的 is_some()` 才覆盖。反过来（让 `None` 覆盖真值）
/// 用户会看到「未声明」而不是真实倍率，且**只在这两条渠道同时存在时才复现**。
fn append_channel(list: &mut Vec<ProductChannel>, c: ProductChannel) {
    for item in list.iter_mut() {
        if item.product == c.product && item.region == c.region {
            if item.credit_multiplier.is_none() && c.credit_multiplier.is_some() {
                item.credit_multiplier = c.credit_multiplier;
            }
            return;
        }
    }
    list.push(c);
}

/// 渠道列表 → 下发形状（Go `channelsToJSON`，`:1238-1256`）。
///
/// # `creditMultiplier` 的**三态**（本函数最容易改错的地方）
///
/// ```text
/// 数字 → 上游声明的倍率
/// null → 上游**未声明**（不知道，**不是**免费）
/// 缺席 → 该平台没有倍率概念（ZCode 目前如此）
/// ```
///
/// 把 `null` 当成「缺席」会让界面显示不出「不知道」这一态；把 `null` 当成 0
/// 会让该计费的模型被统计成免费。
///
/// 同时下发 `region`（单数，界面按区域分组）与 `regions`（复数，旧字段向后
/// 兼容）—— 只发一个会让某一版客户端拿不到区域。
pub fn channels_to_json(chs: &[ProductChannel]) -> Vec<Value> {
    chs.iter()
        .map(|c| {
            let mut e = Map::new();
            e.insert("product".into(), json!(c.product));
            e.insert("label".into(), json!(c.label));
            if !c.region.is_empty() {
                e.insert("region".into(), json!(c.region));
                e.insert("regions".into(), json!([c.region]));
            }
            if c.has_multiplier {
                // nil ⇒ JSON null（不是省略）：省略表示「没有倍率概念」。
                e.insert(
                    "creditMultiplier".into(),
                    match c.credit_multiplier {
                        Some(m) => json!(m),
                        None => Value::Null,
                    },
                );
            }
            Value::Object(e)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 产品模型清单（Go `productModel` / `productModels` / `realmLabelOf`）
// ---------------------------------------------------------------------------

/// 产品自带的模型（Go `productModel`，`:1259-1262`）。
///
/// 与 WorkBuddy 的 `/v3/config` 清单不同，这些只携带**模型名**，没有
/// `context_window` 之类的元数据 —— 见 `merged_model_list` 里对
/// `Qwen3.8-Flash` 的特判。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductModel {
    /// 模型 ID（已 trim）。
    pub id: String,
    /// 产品标识。
    pub product: String,
}

/// 把 `config.product_models` 展开成有序的 [`ProductModel`] 列表
/// （Go `productModels`，`:1314-1332`）。
///
/// ★ **按产品名排序**：Go 侧读的是 `DynamicSnapshot().ProductModels`（一个
/// `map`），遍历顺序随机。不排序的话 `/v1/models` 每次刷新的顺序都可能不同 ——
/// 客户端模型菜单会出现「同一个模型在列表里来回跳」，而且难以复现。
///
/// ★ 读的是**快照**而不是启动时的配置：宿主热重载产品清单后，网关不必重启
/// 就能让新模型出现在 `/v1/models` 里。Rust 侧由调用方传入当前快照。
pub fn product_models_from(products: &HashMap<String, Vec<String>>) -> Vec<ProductModel> {
    let mut keys: Vec<&String> = products.keys().collect();
    keys.sort();

    let mut out = Vec::new();
    for p in keys {
        let Some(ids) = products.get(p) else { continue };
        for raw in ids {
            let id = raw.trim();
            if !id.is_empty() {
                out.push(ProductModel {
                    id: id.to_string(),
                    product: p.clone(),
                });
            }
        }
    }
    out
}

/// 区域 → 用户可读的区域名（Go `realmLabelOf`，`:1271-1280`）。
///
/// ★ 这里返回的正是**下发到 `aliases` 里的组合名所用的那一段**，因此必须与
/// [`crate::resolve_model::is_realm_prefix`] 接受的写法一致 —— 否则我们给出的
/// 写法自己读不回来（用户从界面抄下 `workbuddy:国服:xxx` 会被当成裸模型名，
/// 上游回 `11102 模型不存在`）。
///
/// 国际版那一段直接引用 [`crate::resolve_model::REALM_INTL_CN`] 而不是再写一遍
/// 字面量：两处必须**永远**相同，写两份迟早分叉，而分叉不会编译失败。
fn realm_label_of(region: Region) -> &'static str {
    match region {
        Region::CN => crate::resolve_model::REALM_CN_ALT,
        Region::Intl => crate::resolve_model::REALM_INTL_CN,
        Region::Any => "",
    }
}

/// 取某区域的**动态真值**（没有 / 空 ⇒ 空切片）。
fn dynamic_infos<'a>(
    fetched: &'a HashMap<Region, Option<Vec<ModelInfo>>>,
    region: Region,
) -> &'a [ModelInfo] {
    fetched
        .get(&region)
        .and_then(|v| v.as_deref())
        .unwrap_or(&[])
}

/// `addID`：按**首次出现**顺序登记模型名（Go `:769-775`）。
fn add_id(order: &mut Vec<String>, seen: &mut HashSet<String>, id: &str) {
    if id.is_empty() || seen.contains(id) {
        return;
    }
    seen.insert(id.to_string());
    order.push(id.to_string());
}

/// `addAlias`：给裸模型名挂一个组合名，重复则忽略（Go `:854-861`）。
fn add_alias(map: &mut HashMap<String, Vec<String>>, bare: &str, alias: &str) {
    let list = map.entry(bare.to_string()).or_default();
    if list.iter().any(|have| have == alias) {
        return;
    }
    list.push(alias.to_string());
}

impl CapabilityIndex {
    /// 合并出 `/v1/models` 的完整目录（Go `mergedModelList`，`:750-1178`）。
    ///
    /// 输入全部由调用方提供（本模块不发 HTTP、不读 `AppState`、不碰池子）：
    ///
    /// - `fetched` / `has_account`：同 [`CapabilityIndex::build`]；
    /// - `product_models`：[`product_models_from`] 的结果（Qoder / ZCode 的清单）；
    /// - `product_regions`：某产品账号覆及的区域（Go 侧查池子；无池子时传
    ///   `|_| Vec::new()` 即可，代价只是少发几条区域组合名）。
    ///
    /// # 顺序：国服在前
    ///
    /// 客户端模型菜单常按返回顺序展示，而国服是图片能力与模型覆盖都更完整的
    /// 一侧，排前面更容易被选中。
    ///
    /// # 静态表**只补该区域真值没取到的那一侧**
    ///
    /// 这是修过的真实缺陷（所有者 2026-09-21）：
    ///
    /// ```text
    /// 「国际版既然不支持，为什么你还能跑出来这个模型？接口返回没有就不要
    ///   搞出来，懂吗？」
    /// ```
    ///
    /// 旧代码无条件把两张静态表的**全部**模型加进清单，于是国际版真值里没有的
    /// `hy4-preview`（它只被写进了国际版静态表）照样出现 —— 清单**宣称**了一个
    /// 用不了的模型。静态表是手抄的、必然过期，所以它是「回退」而不是「宣称」：
    /// 有真值时以真值为准。
    ///
    /// 按区域分别判断（而不是「任一区有真值就全不用」）也是必需的：国服可能
    /// 拉到、国际版拉不到，此时只有国际版那一侧需要兜底。
    ///
    /// # 组合名（`aliases` + 独立条目）
    ///
    /// 前缀是**用户指定平台/区域的唯一手段**。只给裸模型名时，用户想在 Qoder 上
    /// 跑某个模型就没法表达 —— 而裸名字的解析规则是「哪个账号可用就用哪个」，
    /// 会误路由（实测 `deepseek-v4.1-flash` 被路由到 Qoder 账号并失败两次）。
    ///
    /// ⚠ 只补**该平台确实提供**的组合，不无脑笛卡尔积 —— 否则菜单里会塞满
    /// 用不了的名字，用户选中后得到「模型不存在」，比不显示更糟。
    pub fn merged_model_list<F>(
        fetched: &HashMap<Region, Option<Vec<ModelInfo>>>,
        has_account: &HashMap<Region, bool>,
        product_models: &[ProductModel],
        product_regions: F,
    ) -> Vec<Value>
    where
        F: Fn(&str) -> Vec<Region>,
    {
        let idx = CapabilityIndex::build(fetched, has_account);
        let cn_infos = dynamic_infos(fetched, Region::CN);
        let intl_infos = dynamic_infos(fetched, Region::Intl);

        // ---- 1. 静态表提供 created / owned_by / context_length 兜底元数据 ----
        //
        // ★ **首个赢**（国服优先），且保留**整条** map（不只是 id）——
        // 下面组装时要按 key 挑 `created` / `owned_by` / `context_length`。
        let mut base: HashMap<String, Value> = HashMap::new();
        for region in [Region::CN, Region::Intl] {
            for m in static_entries(region) {
                let id = m.get("id").and_then(Value::as_str).unwrap_or("");
                if id.is_empty() {
                    continue;
                }
                base.entry(id.to_string()).or_insert(m);
            }
        }

        // ---- 2. 决定清单顺序：两区动态真值（国服在前）→ 静态补齐 → 产品模型 ----
        let mut seen: HashSet<String> = HashSet::new();
        let mut order: Vec<String> = Vec::new();

        for infos in [cn_infos, intl_infos] {
            for mi in infos {
                add_id(&mut order, &mut seen, &mi.id);
            }
        }

        // 静态表：**只补该区域真值没取到的那一侧**。
        //
        // ⚠ 与 Go 的差异：Go 遍历的是一个两元素 map，两侧都缺真值时静态条目的
        // 相对顺序**随机**。这里固定国服先 —— 既符合「国服在前」的既定意图，
        // 又让输出可复现。
        for region in [Region::CN, Region::Intl] {
            let dyn_infos = if region == Region::Intl {
                intl_infos
            } else {
                cn_infos
            };
            if !dyn_infos.is_empty() {
                continue; // 该区域有真值 → 静态表不参与宣称
            }
            for sm in static_table(region) {
                add_id(&mut order, &mut seen, sm.id);
            }
        }

        // 其它产品（Qoder / ZCode）的模型也并入清单 ——
        // 否则用 Qoder 或 ZCode 账号时，客户端模型菜单里看不到它们。
        for pm in product_models {
            add_id(&mut order, &mut seen, &pm.id);
        }

        // ---- 3. 组合名 ----
        let mut aliases_of: HashMap<String, Vec<String>> = HashMap::new();

        // ① 非 WorkBuddy 产品：`平台:模型名` + `平台:区域:模型名`
        for pm in product_models {
            if pm.product.is_empty() {
                continue;
            }
            let bare = pm.id.as_str();
            add_alias(
                &mut aliases_of,
                bare,
                &format!("{}:{}", pm.product, bare),
            );
            // ⚠ 用**归一的显示名**（`国际版` / `国服`）而不是内部标识
            //（`intl` / `cn`）：用户在前缀里写的是前者，写后者解析不出。
            for r in product_regions(&pm.product) {
                let label = realm_label_of(r);
                if !label.is_empty() {
                    add_alias(
                        &mut aliases_of,
                        bare,
                        &format!("{}:{}:{}", pm.product, label, bare),
                    );
                }
            }
        }

        // ② WorkBuddy：与区域无关的 `workbuddy:模型名`（两区并集）。
        {
            let mut ids: HashSet<String> = HashSet::new();
            for infos in [cn_infos, intl_infos] {
                for mi in infos {
                    ids.insert(mi.id.clone());
                }
            }
            // 两区都没拉到真值 → 退回静态表（否则清单会空掉）。
            if ids.is_empty() {
                for region in [Region::CN, Region::Intl] {
                    for sm in static_table(region) {
                        ids.insert(sm.id.to_string());
                    }
                }
            }
            // ⚠ 与 Go 的差异：Go 遍历 map ⇒ 同一条目下这些别名的相对顺序随机。
            // 排序只为让输出可复现，语义不变（`aliases` 是集合语义的列表）。
            let mut sorted: Vec<String> = ids.into_iter().collect();
            sorted.sort();
            for id in sorted {
                add_alias(
                    &mut aliases_of,
                    &id,
                    &format!("{}:{}", PRODUCT_WORKBUDDY, id),
                );
            }
        }

        // ③ WorkBuddy：`平台:区域:模型名` 与**纯区域**前缀（老客户端就在用的写法）。
        //
        // ⚠ 同样按区域判断真值：别名是「这个写法能用」的**承诺**，给真值里没有的
        // 模型编一个区域别名，与把它写进 order 是同一种错误 ——
        // 用户看到 `国际版:hy4-preview` 就会去用它。
        for region in [Region::CN, Region::Intl] {
            let dyn_infos = if region == Region::Intl {
                intl_infos
            } else {
                cn_infos
            };
            let label = realm_label_of(region);
            if label.is_empty() {
                continue;
            }
            if !dyn_infos.is_empty() {
                for mi in dyn_infos {
                    add_alias(
                        &mut aliases_of,
                        &mi.id,
                        &format!("{}:{}:{}", PRODUCT_WORKBUDDY, label, mi.id),
                    );
                    add_alias(&mut aliases_of, &mi.id, &format!("{}:{}", label, mi.id));
                }
                continue;
            }
            for sm in static_table(region) {
                add_alias(
                    &mut aliases_of,
                    sm.id,
                    &format!("{}:{}:{}", PRODUCT_WORKBUDDY, label, sm.id),
                );
                add_alias(&mut aliases_of, sm.id, &format!("{}:{}", label, sm.id));
            }
        }

        // ---- 4. 动态元数据（比静态表准，优先用；首个赢 = 国服优先）----
        let mut dyn_meta: HashMap<String, ModelInfo> = HashMap::new();
        for infos in [cn_infos, intl_infos] {
            for mi in infos {
                dyn_meta.entry(mi.id.clone()).or_insert_with(|| mi.clone());
            }
        }

        // ---- 5. 渠道 ----
        //
        // 倍率从**同一个 capabilityIndex** 取（`idx.by_region[region][id]`），
        // 而不是另起一套查询：那张表已经是「动态真值优先、静态兜底」的权威来源，
        // 重复取数必然分叉。
        let mut channels: HashMap<String, Vec<ProductChannel>> = HashMap::new();

        // WorkBuddy：两区动态清单各成一条渠道，各自带该区倍率。
        for mi in cn_infos {
            append_channel(
                channels.entry(mi.id.clone()).or_default(),
                ProductChannel {
                    product: PRODUCT_WORKBUDDY.to_string(),
                    label: channel_label(PRODUCT_WORKBUDDY).unwrap_or("WorkBuddy").to_string(),
                    region: REGION_CODE_CN.to_string(),
                    credit_multiplier: idx.multiplier(Region::CN, &mi.id),
                    has_multiplier: true,
                },
            );
        }
        for mi in intl_infos {
            append_channel(
                channels.entry(mi.id.clone()).or_default(),
                ProductChannel {
                    product: PRODUCT_WORKBUDDY.to_string(),
                    label: channel_label(PRODUCT_WORKBUDDY).unwrap_or("WorkBuddy").to_string(),
                    region: REGION_CODE_INTL.to_string(),
                    credit_multiplier: idx.multiplier(Region::Intl, &mi.id),
                    has_multiplier: true,
                },
            );
        }

        // 静态表：**只在该区域真值没取到时**才补渠道（真正的「回退」语义）。
        //
        // 为什么倍率留 `None`：手抄表里没有 credits。`has_multiplier` 仍为
        // `true` —— WorkBuddy 这个平台**有**倍率概念，只是这一条没取到值，
        // 界面应显示「—」（不知道）而不是不显示该列。
        for region in [Region::CN, Region::Intl] {
            let dyn_infos = if region == Region::Intl {
                intl_infos
            } else {
                cn_infos
            };
            if !dyn_infos.is_empty() {
                continue;
            }
            let code = if region == Region::Intl {
                REGION_CODE_INTL
            } else {
                REGION_CODE_CN
            };
            for sm in static_table(region) {
                append_channel(
                    channels.entry(sm.id.to_string()).or_default(),
                    ProductChannel {
                        product: PRODUCT_WORKBUDDY.to_string(),
                        label: channel_label(PRODUCT_WORKBUDDY).unwrap_or("WorkBuddy").to_string(),
                        region: code.to_string(),
                        credit_multiplier: idx.multiplier(region, sm.id),
                        has_multiplier: true,
                    },
                );
            }
        }

        // Qoder / ZCode：来自账号池的缓存清单。
        //
        // 这两个产品**不按区域分**（账号池不分 cn/intl），故 region 留空 ——
        // 一个产品一条渠道。
        //
        // 倍率：Qoder 有 `price_factor`（账号级、不分区），但那个值来自账号登录时
        // 的模型清单，**不在本函数的可见范围内**（本函数只拿到模型名清单）。
        // 故这里 `has_multiplier` 留 `false`，界面不显示倍率列 ——
        // 宁可不说，也不编一个可能错的数字。ZCode 目前没有倍率数据源，同上。
        for pm in product_models {
            let label = channel_label(&pm.product)
                .map(str::to_string)
                .unwrap_or_else(|| pm.product.clone());
            append_channel(
                channels.entry(pm.id.clone()).or_default(),
                ProductChannel {
                    product: pm.product.clone(),
                    label,
                    region: String::new(),
                    credit_multiplier: None,
                    has_multiplier: false,
                },
            );
        }

        // ---- 6. 组装裸名条目 ----
        let mut out: Vec<Value> = Vec::with_capacity(order.len());
        let mut bare_entries: HashMap<String, Value> = HashMap::new();

        for id in &order {
            let mut e = Map::new();
            e.insert("id".into(), json!(id));
            e.insert("object".into(), json!("model"));
            e.insert("created".into(), json!(STATIC_CREATED));
            e.insert("owned_by".into(), json!("workbuddy"));

            if let Some(b) = base.get(id) {
                for k in ["created", "owned_by", "context_length", "max_output_tokens"] {
                    if let Some(v) = b.get(k) {
                        e.insert(k.into(), v.clone());
                    }
                }
            }

            // 动态元数据比静态表准 —— 静态表是手抄的，必然过期。
            if let Some(mi) = dyn_meta.get(id) {
                if mi.context_window > 0 {
                    e.insert("context_length".into(), json!(mi.context_window));
                }
                if mi.max_tokens > 0 {
                    e.insert("max_output_tokens".into(), json!(mi.max_tokens));
                }
            }

            if !e.contains_key("context_length") {
                e.insert("context_length".into(), json!(131072i64)); // 通用兜底
            }

            // Qoder 的模型清单只携带模型名，没有 WorkBuddy `/v3/config` 那种
            // `context_window` 元数据。官方 Qoder 客户端明确把 Qwen3.8-Flash 配成
            // 1M；若继续用通用 131072，客户端会把它当成 128K 模型，在压缩阈值上
            // 与官方客户端冲突（提前压缩 = 无谓地丢上下文）。
            if id.eq_ignore_ascii_case("Qwen3.8-Flash") {
                e.insert("context_length".into(), json!(1000000i64));
            }

            for (k, v) in idx.capability_fields_for(id) {
                e.insert(k, v);
            }

            if let Some(ch) = channels.get(id) {
                if !ch.is_empty() {
                    e.insert("channels".into(), Value::Array(channels_to_json(ch)));
                }
            }

            if let Some(al) = aliases_of.get(id) {
                if !al.is_empty() {
                    e.insert("aliases".into(), json!(al));
                }
            }

            let v = Value::Object(e);
            bare_entries.insert(id.clone(), v.clone());
            out.push(v);
        }

        // ---- 7. 组合名也作为**独立条目** ----
        //
        // `aliases` 只是兼容扩展字段，很多 OpenAI 客户端只读取 `data[].id`，
        // 所以组合名必须同时作为独立条目返回。之前只挂 `aliases`，导致用户看到
        // 的 `/v1/models` 只有裸名，无法从模型选择器直接复制 `zcode:glm-5.3`。
        for bare in &order {
            let Some(base_entry) = bare_entries.get(bare) else {
                continue;
            };
            let Some(aliases) = aliases_of.get(bare) else {
                continue;
            };

            for alias in aliases {
                if alias.is_empty() {
                    continue;
                }

                let mut alias_entry = Map::new();
                if let Value::Object(m) = base_entry {
                    for (k, v) in m {
                        if k == "aliases" {
                            continue;
                        }
                        alias_entry.insert(k.clone(), v.clone());
                    }
                }
                alias_entry.insert("id".into(), json!(alias));
                alias_entry.insert("owned_by".into(), json!("gateway-route"));
                alias_entry.insert("route_for".into(), json!(bare));

                // 组合名本身已经指定平台/区域，不能继续展示裸名的全部渠道；
                // 否则 `zcode:glm-5.3` 的模型项仍会显示 WorkBuddy，造成用户
                // 以为这个明确路由还可能落到其它平台。
                let resolved = resolve_model(alias);
                if !(resolved.product.is_empty() && resolved.realm.is_empty()) {
                    if let Some(Value::Array(raw)) = alias_entry.get("channels").cloned() {
                        let want_region = {
                            let r = realm_to_region(&resolved.realm);
                            if r == Region::Any {
                                String::new()
                            } else {
                                r.as_str().to_string()
                            }
                        };
                        let filtered: Vec<Value> = raw
                            .into_iter()
                            .filter(|channel| {
                                let p = channel
                                    .get("product")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                let r = channel
                                    .get("region")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                if !resolved.product.is_empty() && p != resolved.product {
                                    return false;
                                }
                                if !want_region.is_empty() && r != want_region {
                                    return false;
                                }
                                true
                            })
                            .collect();
                        alias_entry.insert("channels".into(), Value::Array(filtered));
                    }
                }

                out.push(Value::Object(alias_entry));
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一条只带 id 的模型信息（其余字段零值 = 上游未声明）。
    fn mi(id: &str) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            ..Default::default()
        }
    }

    /// 只给国服动态真值的索引（国际版无账号 ⇒ 静态兜底 + `MissingAccounts`）。
    fn idx_cn_only(cn: Vec<ModelInfo>) -> CapabilityIndex {
        let mut fetched: HashMap<Region, Option<Vec<ModelInfo>>> = HashMap::new();
        fetched.insert(Region::CN, Some(cn));
        fetched.insert(Region::Intl, None);
        let mut has: HashMap<Region, bool> = HashMap::new();
        has.insert(Region::CN, true);
        has.insert(Region::Intl, false);
        CapabilityIndex::build(&fetched, &has)
    }

    // -- CreditMultiplier 三态 -------------------------------------------------

    #[test]
    fn credit_multiplier_is_three_state() {
        let mut billed = mi("deepseek-v4.1-flash");
        billed.credit_multiplier = Some(0.03);
        let mut free = mi("hy3");
        free.credit_multiplier = Some(0.0);
        // 上游整条缺失该字段 ⇒ None（**不是**免费）。
        let undeclared = mi("auto");

        let idx = idx_cn_only(vec![billed, free, undeclared]);

        assert_eq!(
            idx.multiplier(Region::CN, "deepseek-v4.1-flash"),
            Some(0.03),
            "★ 上游声明的倍率必须原样透出"
        );
        assert_eq!(
            idx.multiplier(Region::CN, "hy3"),
            Some(0.0),
            "★ 免费是 0，不是 null"
        );
        assert_eq!(
            idx.multiplier(Region::CN, "auto"),
            None,
            "★ 未声明必须是 None —— 当成 0（免费）会把该计费的统计成免费"
        );
        assert_eq!(
            idx.multiplier(Region::CN, "不存在的模型"),
            None,
            "★ 未收录 ⇒ None，绝不编造"
        );
    }

    #[test]
    fn credit_multiplier_is_per_region_not_per_model() {
        // 实测 2026-09-18：同名模型两区结论相反。
        let mut cn_flash = mi("deepseek-v4.1-flash");
        cn_flash.credit_multiplier = Some(0.03);
        let mut intl_flash = mi("deepseek-v4.1-flash");
        intl_flash.credit_multiplier = Some(0.0);

        let mut fetched: HashMap<Region, Option<Vec<ModelInfo>>> = HashMap::new();
        fetched.insert(Region::CN, Some(vec![cn_flash]));
        fetched.insert(Region::Intl, Some(vec![intl_flash]));
        let idx = CapabilityIndex::build(&fetched, &HashMap::new());

        assert_eq!(idx.multiplier(Region::CN, "deepseek-v4.1-flash"), Some(0.03));
        assert_eq!(
            idx.multiplier(Region::Intl, "deepseek-v4.1-flash"),
            Some(0.0)
        );
    }

    // -- known_region / 静态兜底 ----------------------------------------------

    #[test]
    fn known_region_only_set_for_dynamic_truth() {
        let idx = idx_cn_only(vec![mi("glm-5.3")]);
        assert!(idx.is_known_region(Region::CN), "拉到了非空清单 ⇒ known");
        assert!(
            !idx.is_known_region(Region::Intl),
            "★ 静态兜底**不算**真值 —— 拿它断言「国际版没有某模型」是错的"
        );
    }

    #[test]
    fn empty_dynamic_list_is_treated_as_missing() {
        // Some(空) 与 None 等价：都回落静态表。
        let mut fetched: HashMap<Region, Option<Vec<ModelInfo>>> = HashMap::new();
        fetched.insert(Region::CN, Some(Vec::new()));
        fetched.insert(Region::Intl, None);
        let idx = CapabilityIndex::build(&fetched, &HashMap::new());

        assert!(!idx.is_known_region(Region::CN));
        assert!(
            idx.lookup("glm-5.3", Region::CN).is_some(),
            "★ 空清单也必须回落静态表，否则整批模型的能力字段都会消失"
        );
        assert!(idx.lookup("glm-5.3", Region::CN).unwrap().from_static);
    }

    #[test]
    fn static_fallback_marks_from_static_and_fabricates_nothing() {
        let idx = idx_cn_only(vec![mi("glm-5.3")]);
        let c = idx
            .lookup("glm-5.3", Region::Intl)
            .expect("静态表里国际版有 glm-5.3");
        assert!(c.from_static);
        assert!(c.present);
        // ★ 实测结论**优先于静态表**：Go `capability.go:528` 的
        // `SupportsImages: measuredOverride(mi.ID, region, mi.SupportsImages)` 在
        // `add()` 里，而静态路径（`:564 add(region, infosFromStatic(static), false)`）
        // 走的**是同一个闭包** —— 所以 `infosFromStatic` 标注的 `&true` 会被实测
        // 覆盖掉。国际版 glm-5.3 实测读不到图（`measured.go:116-126`），
        // 故此处必须是 `Some(false)`。
        //
        // 若照抄静态表的 `true`，网关就会宣称国际版能读图，把带图片的请求推向一个
        // 读不到图的后端 —— 客户端看起来一切正常，只是模型说「我看不见图片」。
        assert_eq!(
            c.supports_images,
            Some(false),
            "★ 实测结论必须覆盖静态表的统一标注"
        );
        assert_eq!(
            c.upstream_images,
            Some(true),
            "★ upstream_images 保留**静态表原话**（true），供对比展示：\
             「表里写的」与「实测的」不一致时最需要能一眼看出来"
        );
        assert!(
            c.efforts.is_empty() && c.default_effort.is_empty(),
            "★ 静态表不编造档位"
        );
        assert_eq!(c.credit_multiplier, None, "★ 静态表不编造倍率");
    }

    #[test]
    fn static_fallback_marks_supports_images_true_for_unmeasured_models() {
        // ★ 必须用一个**未实测**的模型才能看到静态表真正的标注：
        // `measured_for_region` 只认 glm-5.2/5.3，其余一律 `Unmeasured` ⇒
        // `measured_override` 原样透传静态表的 `Some(true)`。
        let idx = idx_cn_only(vec![mi("glm-5.3")]);
        let c = idx
            .lookup("default-model", Region::Intl)
            .expect("静态表里国际版有 default-model");
        assert!(c.from_static);
        assert_eq!(
            c.supports_images,
            Some(true),
            "静态表统一标注支持图片（否则回退期间图片能力整个消失）"
        );
        assert_eq!(c.upstream_images, Some(true));
    }

    #[test]
    fn dynamic_entry_is_not_from_static() {
        let idx = idx_cn_only(vec![mi("glm-5.3")]);
        let c = idx.lookup("glm-5.3", Region::CN).unwrap();
        assert!(!c.from_static);
        assert_eq!(
            c.supports_images,
            Some(true),
            "★ 上游未声明（None），但国服 glm-5.3 实测能读图 ⇒ 实测把它纠正成 true\
             （`measured.go:134` 注释：实测支持 → 覆盖成 true，上游漏标时也能纠正）"
        );
        assert_eq!(
            c.upstream_images, None,
            "★ upstream_images 仍是上游原话（未声明），不被实测改写"
        );
    }

    #[test]
    fn unmeasured_dynamic_entry_passes_upstream_three_state_through() {
        // 未实测的模型：上游三态必须原样透传（`measured_override` 的 default 分支）。
        let mut declared_true = mi("m-declared-true");
        declared_true.supports_images = Some(true);
        let mut declared_false = mi("m-declared-false");
        declared_false.supports_images = Some(false);
        let undeclared = mi("m-undeclared");

        let idx = idx_cn_only(vec![declared_true, declared_false, undeclared]);

        let img = |id: &str| {
            idx.lookup(id, Region::CN)
                .unwrap_or_else(|| panic!("缺 {id}"))
                .supports_images
        };
        assert_eq!(img("m-declared-true"), Some(true));
        assert_eq!(img("m-declared-false"), Some(false));
        assert_eq!(
            img("m-undeclared"),
            None,
            "★ 未实测且上游未声明 ⇒ None（**不是** false）"
        );
    }

    #[test]
    fn empty_id_is_skipped() {
        let idx = idx_cn_only(vec![mi(""), mi("glm-5.3")]);
        assert!(idx.lookup("", Region::CN).is_none(), "空 id 不得变成幽灵条目");
    }

    // -- gap_reason 两种 cause -------------------------------------------------

    #[test]
    fn gap_cause_distinguishes_missing_accounts_from_fetch_failure() {
        let mut fetched: HashMap<Region, Option<Vec<ModelInfo>>> = HashMap::new();
        fetched.insert(Region::CN, None);
        fetched.insert(Region::Intl, None);
        let mut has: HashMap<Region, bool> = HashMap::new();
        has.insert(Region::CN, true); // 有账号但没拉到
        has.insert(Region::Intl, false); // 一个账号都没有
        let idx = CapabilityIndex::build(&fetched, &has);

        let cn = idx.gap_reason_for(Region::CN).unwrap();
        assert_eq!(
            cn.cause,
            GapCause::FetchFailed,
            "★ 有账号却没拉到 ⇒ 绝不能给「补账号」的建议"
        );
        assert!(cn.reason.contains("自动重试"));

        let intl = idx.gap_reason_for(Region::Intl).unwrap();
        assert_eq!(intl.cause, GapCause::MissingAccounts);
        assert!(intl.reason.contains("没有国际版的可用账号"));
    }

    #[test]
    fn gap_reason_only_present_when_region_unknown() {
        let idx = idx_cn_only(vec![mi("glm-5.3")]);
        assert!(idx.gap_reason_for(Region::CN).is_none(), "有真值就无缺口");
        assert!(idx.gap_reason_for(Region::Intl).is_some());
    }

    // -- Present / 图片能力三态 -----------------------------------------------

    #[test]
    fn present_distinguishes_listed_but_text_only_from_not_listed() {
        let mut text_only = mi("text-only");
        text_only.supports_images = Some(false);
        let idx = idx_cn_only(vec![text_only]);

        let c = idx.lookup("text-only", Region::CN).unwrap();
        assert!(c.present, "★ 列了但不支持图片：present = true");
        assert_eq!(c.supports_images, Some(false));

        assert!(
            idx.lookup("never-listed", Region::CN).is_none(),
            "★ 压根没列：lookup 返回 None（与 present=false 不是一回事）"
        );
    }

    #[test]
    fn image_fields_three_state() {
        let yes = model_capability_fields(Some(true)).unwrap();
        assert_eq!(yes["supportsImages"], json!(true));
        assert_eq!(yes["input_modalities"], json!(["text", "image"]));
        assert_eq!(yes["capabilities"]["vision"], json!(true));
        assert_eq!(yes["architecture"]["modality"], json!("text+image->text"));
        assert_eq!(yes["tags"], json!(["vision"]));
        assert_eq!(yes["modalities"]["output"], json!(["text"]));

        let no = model_capability_fields(Some(false)).unwrap();
        assert_eq!(no["supportsImages"], json!(false));
        assert_eq!(no["capabilities"]["vision"], json!(false));

        assert!(
            model_capability_fields(None).is_none(),
            "★ 未声明 ⇒ 不下发任何键（宁可不写，也不要谎报成纯文本）"
        );
    }

    #[test]
    fn measured_override_wins_over_upstream_for_glm() {
        // 上游对国际版 glm-5.3 报 true，实测读不到图（图片被换成固定占位符）。
        let mut glm = mi("glm-5.3");
        glm.supports_images = Some(true);
        let mut fetched: HashMap<Region, Option<Vec<ModelInfo>>> = HashMap::new();
        fetched.insert(Region::CN, Some(vec![glm.clone()]));
        fetched.insert(Region::Intl, Some(vec![glm]));
        let idx = CapabilityIndex::build(&fetched, &HashMap::new());

        let cn = idx.lookup("glm-5.3", Region::CN).unwrap();
        assert_eq!(cn.supports_images, Some(true));
        assert_eq!(cn.upstream_images, Some(true), "上游原话仍留着供诊断");

        let intl = idx.lookup("glm-5.3", Region::Intl).unwrap();
        assert_eq!(
            intl.supports_images,
            Some(false),
            "★ 实测必须压过上游声明 —— 否则客户端会发出必然静默降级的请求"
        );
        assert_eq!(intl.upstream_images, Some(true), "上游原话不变");
    }

    // -- SupportsReasoning：nil ≠ false --------------------------------------

    #[test]
    fn supports_reasoning_nil_is_not_false() {
        let undeclared = mi("a");
        let mut declared_false = mi("b");
        declared_false.supports_reasoning = Some(false);
        let mut declared_true = mi("c");
        declared_true.supports_reasoning = Some(true);

        let idx = idx_cn_only(vec![undeclared, declared_false, declared_true]);

        assert_eq!(idx.lookup("a", Region::CN).unwrap().supports_reasoning, None);
        assert_eq!(
            idx.lookup("b", Region::CN).unwrap().supports_reasoning,
            Some(false)
        );
        assert_eq!(
            idx.lookup("c", Region::CN).unwrap().supports_reasoning,
            Some(true)
        );
        assert_ne!(
            idx.lookup("a", Region::CN).unwrap().supports_reasoning,
            idx.lookup("b", Region::CN).unwrap().supports_reasoning,
            "★ 未声明与 false 必须可区分 —— 混同会让本可用的思考能力被关掉"
        );
    }

    #[test]
    fn can_disable_thinking_is_emitted_in_both_spellings() {
        let mut m = mi("only-reasoning");
        m.supports_images = Some(true);
        m.can_disable_thinking = Some(false);
        let idx = idx_cn_only(vec![m]);

        let f = idx.capability_fields_for("only-reasoning");
        assert_eq!(f["can_disable_thinking"], json!(false));
        assert_eq!(f["canDisableThinking"], json!(false));
    }

    // -- 思考档位三情形 --------------------------------------------------------

    #[test]
    fn reasoning_case_a_lists_declared_range_verbatim() {
        let mut m = mi("hy3");
        m.efforts = vec!["low".into(), "high".into()];
        m.default_effort = "high".into();
        let idx = idx_cn_only(vec![m]);

        let f = idx.capability_fields_for("hy3");
        assert_eq!(f["supported_efforts"], json!(["low", "high"]));
        assert_eq!(f["supportedEfforts"], json!(["low", "high"]));
        assert_eq!(f["reasoning_efforts"], json!(["low", "high"]));
        assert_eq!(f["reasoning"]["supported_efforts"], json!(["low", "high"]));
        assert_eq!(f["default_effort"], json!("high"));
        assert_eq!(f["defaultEffort"], json!("high"));
        assert_eq!(f["default_reasoning_effort"], json!("high"));
        assert_eq!(f["reasoning"]["default_effort"], json!("high"));
        assert_eq!(f["supports_reasoning"], json!(true));
        assert_eq!(
            f["reasoning_fixed"],
            json!(false),
            "★ 「声明了」不等于「只有这些可用」⇒ 不能标成固定档"
        );
        assert!(
            f["reasoning"].get("range_undeclared").is_none(),
            "情形 A 不标 range_undeclared"
        );
    }

    #[test]
    fn reasoning_case_b_uses_standard_ladder_and_marks_undeclared() {
        let mut m = mi("deepseek-v4.1-flash");
        m.supports_images = Some(true);
        m.efforts = Vec::new();
        m.default_effort = "max".into();
        let idx = idx_cn_only(vec![m]);

        let f = idx.capability_fields_for("deepseek-v4.1-flash");
        assert_eq!(
            f["supported_efforts"],
            json!(["off", "minimal", "low", "medium", "high", "xhigh", "max"]),
            "标准阶梯按强度升序"
        );
        assert_eq!(f["reasoning"]["range_undeclared"], json!(true));
        assert_eq!(f["reasoning_range_undeclared"], json!(true));
        assert_eq!(f["reasoningRangeUndeclared"], json!(true));
        assert_eq!(f["reasoning"]["declared_by_upstream"]["low"], json!(true));
        assert_eq!(
            f["reasoning"]["declared_by_upstream"]["off"],
            json!(false),
            "off 从未被任何模型声明过"
        );
        assert_eq!(f["upstream_declared_efforts"]["max"], json!(true));
        assert_eq!(f["default_effort"], json!("max"));
        assert_eq!(
            f["reasoning_fixed"],
            json!(false),
            "★ 情形 B 绝不能标成固定档 —— 实测调 max 确实让推理变长"
        );
    }

    #[test]
    fn reasoning_case_c_emits_nothing() {
        // 既没有档位数组，也没有默认档，也没有图片能力 ⇒ 整个 map 为空。
        let idx = idx_cn_only(vec![mi("silent")]);
        let f = idx.capability_fields_for("silent");
        assert!(f.is_empty(), "★ 不下发字段，而不是下发「不支持」");
    }

    #[test]
    fn reasoning_is_orthogonal_to_image_capability() {
        // 图片能力未声明（None），但有默认档 ⇒ 思考字段仍必须下发。
        let mut m = mi("fixed-only");
        m.default_effort = "medium".into();
        let idx = idx_cn_only(vec![m]);

        let f = idx.capability_fields_for("fixed-only");
        assert!(
            f.get("supportsImages").is_none(),
            "图片能力未声明 ⇒ 不下发"
        );
        assert_eq!(f["default_effort"], json!("medium"));
        assert_eq!(f["reasoning"]["range_undeclared"], json!(true));
    }

    #[test]
    fn default_effort_is_trimmed() {
        let mut m = mi("padded");
        m.efforts = vec!["low".into()];
        m.default_effort = "  low  ".into();
        let idx = idx_cn_only(vec![m]);
        assert_eq!(
            idx.capability_fields_for("padded")["default_effort"],
            json!("low")
        );
    }

    /// 上游声明了档位、但**没声明默认档**时，不得用 `efforts[0]` 猜测填充。
    ///
    /// ★ 这条覆盖的是「宁可不写，也不要凭空编造」：客户端会把默认档当成
    /// 用户没显式指定时的实际取值。若拿首档冒充，用户看到的是「我没调档位，
    /// 却按 low 跑」，而真实默认档可能是 high —— 与 `server.rs` 里那份
    /// 2026-10-01 删除的陈旧副本不同，本实现是**下发档位列表但不填默认档**，
    /// 而不是整组不下发。
    #[test]
    fn default_effort_is_not_fabricated() {
        let mut m = mi("no-default");
        m.efforts = vec!["low".into(), "high".into()];
        // default_effort 留空（上游未声明）。
        let idx = idx_cn_only(vec![m]);

        let f = idx.capability_fields_for("no-default");
        assert_eq!(f["supported_efforts"], json!(["low", "high"]));
        assert!(
            f.get("default_effort").is_none(),
            "★ 不得用 efforts[0] 冒充默认档"
        );
        assert!(f["reasoning"].get("default_effort").is_none());
        assert_ne!(f.get("default_effort"), Some(&json!("low")));
        // 档位是上游声明的 ⇒ 情形 A，不标 range_undeclared。
        assert!(f["reasoning"].get("range_undeclared").is_none());
    }

    #[test]
    fn effort_list_is_copied_not_aliased() {
        let mut m = mi("shared");
        m.efforts = vec!["low".into(), "high".into()];
        m.default_effort = "high".into();
        let idx = idx_cn_only(vec![m]);

        let f = idx.capability_fields_for("shared");
        // 就地改动返回值，不得影响索引里那份。
        let mut f = f;
        f.insert("supported_efforts".into(), json!(["tampered"]));
        let again = idx.capability_fields_for("shared");
        assert_eq!(again["supported_efforts"], json!(["low", "high"]));
        assert_eq!(
            idx.lookup("shared", Region::CN).unwrap().efforts,
            vec!["low".to_string(), "high".to_string()]
        );
    }

    // -- region_fields / 说明文案 ---------------------------------------------

    #[test]
    fn region_fields_absent_when_both_regions_have_truth() {
        let mut fetched: HashMap<Region, Option<Vec<ModelInfo>>> = HashMap::new();
        fetched.insert(Region::CN, Some(vec![mi("glm-5.3")]));
        fetched.insert(Region::Intl, Some(vec![mi("glm-5.3")]));
        let idx = CapabilityIndex::build(&fetched, &HashMap::new());

        let f = idx.capability_fields_for("glm-5.3");
        assert!(f.get("supported_regions").is_none(), "两区都有 ⇒ 无需说明");
        assert!(f.get("region_note").is_none());
    }

    #[test]
    fn region_fields_static_side_does_not_count_as_support() {
        // 国服动态有、国际版只有静态兜底（且静态表里确实有 glm-5.3）⇒
        // 静态不算「支持」，故走 unverified 分支而不是「仅国服存在」。
        let mut cn = mi("glm-5.3");
        cn.supports_images = Some(true);
        let mut fetched: HashMap<Region, Option<Vec<ModelInfo>>> = HashMap::new();
        fetched.insert(Region::CN, Some(vec![cn]));
        fetched.insert(Region::Intl, None);
        // ★ 国际版**有账号**但这次没拉到 ⇒ FetchFailed（而不是 MissingAccounts）。
        // 这两种原因的行动建议**相反**，故必须分别覆盖。
        let mut has = HashMap::new();
        has.insert(Region::CN, true);
        has.insert(Region::Intl, true);
        let idx = CapabilityIndex::build(&fetched, &has);

        let f = idx.capability_fields_for("glm-5.3");
        assert_eq!(f["supported_regions"], json!(["cn"]));
        assert_eq!(
            f["unverified_regions"],
            json!(["intl"]),
            "★ 静态表只提供信息、不否定存在 ⇒ 只能说「未检测到真值」"
        );
        let note = f["region_note"].as_str().unwrap();
        assert!(note.contains("无法确认"), "note = {note}");
        assert!(
            note.contains("正在自动重试"),
            "★ 有账号、只是这次没拉到 ⇒ 必须是「正在自动重试」\
             （所有者 2026-09-20：「这应该是自动的,而不是需要人手动同步,你懂吗?」）；\
             note = {note}"
        );
        assert!(
            !note.contains("补齐"),
            "★ 账号是够的 ⇒ 绝不能建议补账号（用户会去加一个不需要的账号）；note = {note}"
        );
    }

    #[test]
    fn region_fields_static_side_with_no_accounts_says_add_accounts() {
        // 与上一条同形，但国际版**一个账号都没有** ⇒ MissingAccounts。
        // ★ 两种 cause 的行动建议相反，分派靠**枚举**而不是解析文案 ——
        // 文案改一个字就失效，所以这里断言的是枚举分派的结果。
        let mut cn = mi("glm-5.3");
        cn.supports_images = Some(true);
        let idx = idx_cn_only(vec![cn]); // Intl has_account = false

        let f = idx.capability_fields_for("glm-5.3");
        let note = f["region_note"].as_str().unwrap();
        assert!(note.contains("无法确认"), "note = {note}");
        assert!(
            note.contains("补齐国际版账号后即可确认。"),
            "★ 一个账号都没有 ⇒ 这时才该建议补账号；note = {note}"
        );
        assert!(
            !note.contains("正在自动重试"),
            "★ 没有账号是重试解决不了的，说「正在重试」会让用户白等；note = {note}"
        );
    }

    #[test]
    fn region_note_says_only_this_region_has_it() {
        // 国际版动态有、国服动态也有但**没有**该模型 ⇒ 情形 B（另一区有真值）。
        let mut cn = mi("glm-5.3");
        cn.supports_images = Some(true);
        let mut intl = mi("gpt-5.6-sol");
        // ★ 该模型至少要有一个能力字段，否则 `capability_fields_for` 会先早退
        //（Go `:1379-1381`：`fields` 与 `reasoning` 都为空 ⇒ 连区域说明都不下发）。
        intl.supports_images = Some(true);
        let mut fetched: HashMap<Region, Option<Vec<ModelInfo>>> = HashMap::new();
        fetched.insert(Region::CN, Some(vec![cn]));
        fetched.insert(Region::Intl, Some(vec![intl]));
        let idx = CapabilityIndex::build(&fetched, &HashMap::new());

        let f = idx.capability_fields_for("gpt-5.6-sol");
        assert_eq!(f["supported_regions"], json!(["intl"]));
        assert!(
            f.get("unverified_regions").is_none(),
            "另一区有真值且确实没有它 ⇒ 这是**已确认**，不是未验证"
        );
        let note = f["region_note"].as_str().unwrap();
        assert!(note.contains("11102"), "note = {note}");
        assert!(note.contains("仅在国际版上游存在"), "note = {note}");
    }

    #[test]
    fn unverified_note_for_missing_accounts_suggests_adding_accounts() {
        let gap = RegionGap {
            cause: GapCause::MissingAccounts,
            reason: "账号池里没有国服的可用账号".into(),
        };
        let note = unverified_note(REGION_CODE_INTL, REGION_CODE_CN, &gap);
        assert!(note.contains("补齐国服账号后即可确认。"), "note = {note}");
    }

    #[test]
    fn unverified_note_for_fetch_failure_never_says_manual_refresh() {
        let gap = RegionGap {
            cause: GapCause::FetchFailed,
            reason: "国服账号清单本次未拉到（上游暂时不可达，会自动重试）".into(),
        };
        let note = unverified_note(REGION_CODE_INTL, REGION_CODE_CN, &gap);
        assert!(note.contains("正在自动重试"), "note = {note}");
        assert!(note.contains("无需手动刷新"), "note = {note}");
        assert!(
            !note.contains("补齐"),
            "★ 有账号却没拉到 ⇒ 说「补账号」是错的建议；note = {note}"
        );
    }

    #[test]
    fn region_code_helpers() {
        assert_eq!(other_region_code(REGION_CODE_CN), REGION_CODE_INTL);
        assert_eq!(other_region_code(REGION_CODE_INTL), REGION_CODE_CN);
        assert_eq!(region_label(REGION_CODE_CN), "国服");
        assert_eq!(region_label(REGION_CODE_INTL), "国际版");
        assert_eq!(region_by_code(REGION_CODE_INTL), Region::Intl);
        assert_eq!(
            region_by_code("不认识"),
            Region::CN,
            "未知码按国服（保守）"
        );
    }

    #[test]
    fn region_codes_match_region_as_str() {
        assert_eq!(REGION_CODE_CN, Region::CN.as_str());
        assert_eq!(REGION_CODE_INTL, Region::Intl.as_str());
    }

    // -- 静态表 ---------------------------------------------------------------

    #[test]
    fn static_tables_have_expected_sizes() {
        assert_eq!(static_model_ids(Region::CN).len(), 16);
        assert_eq!(static_model_ids(Region::Intl).len(), 20);
    }

    /// 静态表内容快照：宿主侧（`ai-gateway-core` 的 `static_models()`）
    /// 是**照抄**这两张表的并集，任何一侧改动都必须同步。
    ///
    /// # 为什么要有它（2026-10-06）
    ///
    /// core 的 `/v1/models` 回退清单此前是手工维护的另一份拷贝，已经与
    /// 这两张表分叉：缺 `hy3`/`glm-5.1`/`kimi-k2.7`（网关没启动时选不到），
    /// 多 `deepseek-v4-flash`/`glm-4.7`/`hunyuan-chat`（已下线，选了必 11102）。
    /// 两侧刻意不共享代码（网关不依赖 core，理由见 router 的 Cargo.toml），
    /// 于是用**内容快照**互相钉住：这里钉住本表，core 那边钉住并集。
    ///
    /// ⚠ 本测试**只读**，不改表；改表的人必须同时改 core 那张。
    #[test]
    fn static_tables_content_snapshot() {
        assert_eq!(
            static_model_ids(Region::CN),
            vec![
                "auto",
                "hy4-preview",
                "hy3",
                "hy3-x",
                "deepseek-v4.1-flash",
                "deepseek-v4-pro",
                "glm-5.3",
                "glm-5.3-flash",
                "glm-5.2",
                "glm-5.1",
                "glm-5v-turbo",
                "minimax-m3",
                "kimi-k3-1",
                "kimi-k2.8-preview",
                "kimi-k2.7",
                "kimi-k2.6",
            ],
            "国服静态表变了：core 的 static_models() 前 16 条必须同步"
        );
        assert_eq!(
            static_model_ids(Region::Intl),
            vec![
                "default-model",
                "fast-model",
                "balanced-model",
                "primary-model",
                "deep-model",
                "hy4-preview-f",
                "hy3",
                "deepseek-v4.1-flash",
                "gpt-6-astra",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-5.5",
                "gpt-5.4",
                "gemini-3.5-flash",
                "glm-5.3",
                "glm-5.2",
                "kimi-k3",
                "kimi-k2.8-preview",
                "kimi-k2.6",
            ],
            "国际版静态表变了：core 的 static_models() 后 14 条必须同步"
        );
        // 两表重叠恰好 6 个 ⇒ 并集 30（core 那张表的条目数断言依赖这个数）。
        let cn = static_model_ids(Region::CN);
        let intl = static_model_ids(Region::Intl);
        let overlap = cn.iter().filter(|id| intl.contains(id)).count();
        assert_eq!(overlap, 6, "两表重叠数变了 ⇒ core 并集条目数（30）也要改");
    }

    #[test]
    fn static_models_intl_has_no_cn_only_model() {
        let intl = static_model_ids(Region::Intl);
        // ★ hy4-preview 是**国服专有**：写进国际版表会让 /v1/models 宣称
        // 国际版也能调它，而国际版根本调不到（所有者 2026-09-21 报的缺陷）。
        for cn_only in [
            "hy4-preview",
            "hy3-x",
            "deepseek-v4-pro",
            "glm-5.1",
            "glm-5v-turbo",
            "minimax-m3",
            "kimi-k3-1",
            "kimi-k2.7",
        ] {
            assert!(
                !intl.iter().any(|id| id == cn_only),
                "★ {cn_only} 不得出现在国际版静态表"
            );
        }
        // 反向：国服表**保留** hy4-preview（国服真值确实有）。
        assert!(static_model_ids(Region::CN).iter().any(|id| id == "hy4-preview"));
        // 国际版有 hy4-preview-f（注意 -f 后缀，是**另一个**模型）。
        assert!(intl.iter().any(|id| id == "hy4-preview-f"));
    }

    #[test]
    fn static_entries_carry_metadata_and_image_fields() {
        let cn = static_entries(Region::CN);
        let glm = cn
            .iter()
            .find(|e| e["id"] == json!("glm-5.3"))
            .expect("国服静态表必须有 glm-5.3");
        assert_eq!(glm["object"], json!("model"));
        assert_eq!(glm["created"], json!(STATIC_CREATED));
        assert_eq!(glm["owned_by"], json!("workbuddy"));
        assert_eq!(glm["context_length"], json!(1000000));
        assert_eq!(glm["supportsImages"], json!(true));
        assert_eq!(glm["tags"], json!(["vision"]));

        let intl = static_entries(Region::Intl);
        let glm_intl = intl
            .iter()
            .find(|e| e["id"] == json!("glm-5.3"))
            .expect("国际版静态表必须有 glm-5.3");
        assert_eq!(
            glm_intl["owned_by"],
            json!("workbuddy-intl"),
            "★ owned_by 按表区分（不要硬编码 workbuddy）"
        );
    }

    #[test]
    fn static_entries_for_any_region_is_empty() {
        assert!(static_entries(Region::Any).is_empty());
        assert!(static_model_ids(Region::Any).is_empty());
    }

    // -- 标准阶梯 -------------------------------------------------------------

    #[test]
    fn standard_efforts_is_rank_ascending() {
        assert_eq!(
            standard_efforts(),
            vec!["off", "minimal", "low", "medium", "high", "xhigh", "max"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn upstream_declared_effort_normalizes_input() {
        assert!(upstream_declared_effort("LOW"));
        assert!(upstream_declared_effort("  max  "));
        assert!(!upstream_declared_effort("off"));
        assert!(!upstream_declared_effort("minimal"));
        assert!(!upstream_declared_effort("bogus"));
    }

    // =======================================================================
    // 批次 2：渠道 / 组合名 / merged_model_list
    // =======================================================================

    /// 两区都有动态真值的 `fetched`。
    fn fetched_both(
        cn: Vec<ModelInfo>,
        intl: Vec<ModelInfo>,
    ) -> HashMap<Region, Option<Vec<ModelInfo>>> {
        let mut fetched = HashMap::new();
        fetched.insert(Region::CN, Some(cn));
        fetched.insert(Region::Intl, Some(intl));
        fetched
    }

    /// 两区都有账号。
    fn has_both() -> HashMap<Region, bool> {
        let mut has = HashMap::new();
        has.insert(Region::CN, true);
        has.insert(Region::Intl, true);
        has
    }

    /// 按 id 取清单里的条目（没有就 panic，让失败信息直接指出缺了哪个模型）。
    fn entry<'a>(list: &'a [Value], id: &str) -> &'a Value {
        list.iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(id))
            .unwrap_or_else(|| panic!("★ 清单里没有 {id}"))
    }

    fn has_entry(list: &[Value], id: &str) -> bool {
        list.iter()
            .any(|e| e.get("id").and_then(Value::as_str) == Some(id))
    }

    /// 条目的 `channels` 数组（缺席 ⇒ panic，因为本模块的条目都该有渠道）。
    fn channels_of<'a>(e: &'a Value) -> &'a Vec<Value> {
        e.get("channels")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("★ 条目没有 channels: {e}"))
    }

    fn chan(product: &str, region: &str, mult: Option<f64>, has_mult: bool) -> ProductChannel {
        ProductChannel {
            product: product.to_string(),
            label: channel_label(product).unwrap_or(product).to_string(),
            region: region.to_string(),
            credit_multiplier: mult,
            has_multiplier: has_mult,
        }
    }

    // -- append_channel --------------------------------------------------------

    #[test]
    fn append_channel_dedupes_on_product_and_region() {
        let mut list = Vec::new();
        append_channel(&mut list, chan("workbuddy", "cn", Some(0.03), true));
        append_channel(&mut list, chan("workbuddy", "cn", Some(0.03), true));
        assert_eq!(list.len(), 1, "同 (产品, 区域) 只留一条");

        // ★ 同产品、不同区域必须**各自**保留：倍率按区域不同，合并必然显示错一个。
        append_channel(&mut list, chan("workbuddy", "intl", Some(0.0), true));
        assert_eq!(list.len(), 2, "★ 区域不同就不能合并 —— 合并会丢一个倍率");

        // 不分区产品（region 空串）同样按 (产品, 区域) 去重。
        append_channel(&mut list, chan("qoder", "", None, false));
        append_channel(&mut list, chan("qoder", "", None, false));
        assert_eq!(list.len(), 3, "不分区产品只留一条");
    }

    #[test]
    fn append_channel_never_lets_none_overwrite_a_real_multiplier() {
        // 先有真值，后到未声明 ⇒ 真值保留。
        let mut list = Vec::new();
        append_channel(&mut list, chan("workbuddy", "cn", Some(0.03), true));
        append_channel(&mut list, chan("workbuddy", "cn", None, true));
        assert_eq!(
            list[0].credit_multiplier,
            Some(0.03),
            "★ 后到的 None 绝不能覆盖已声明的真值 —— 用户会看到「未声明」而不是真实倍率"
        );

        // 先未声明，后到真值 ⇒ 补上。
        let mut list = Vec::new();
        append_channel(&mut list, chan("workbuddy", "cn", None, true));
        append_channel(&mut list, chan("workbuddy", "cn", Some(0.0), true));
        assert_eq!(
            list[0].credit_multiplier,
            Some(0.0),
            "★ 0 是**有效**值（免费），必须能补进来"
        );
    }

    // -- channels_to_json 三态 -------------------------------------------------

    #[test]
    fn channels_to_json_three_states_of_multiplier() {
        let out = channels_to_json(&[
            chan("workbuddy", "cn", Some(0.03), true),  // 数字
            chan("workbuddy", "intl", None, true),      // null（未声明）
            chan("zcode", "", None, false),             // 缺席（无倍率概念）
        ]);
        assert_eq!(out.len(), 3);

        assert_eq!(out[0]["creditMultiplier"], 0.03);
        assert_eq!(out[0]["product"], "workbuddy");
        assert_eq!(out[0]["label"], "WorkBuddy");
        assert_eq!(out[0]["region"], "cn");
        assert_eq!(out[0]["regions"], json!(["cn"]));

        assert!(
            out[1].get("creditMultiplier").is_some(),
            "★ 未声明必须**显式**下发 null（key 缺席会被读成「没有倍率概念」）"
        );
        assert!(out[1]["creditMultiplier"].is_null());
        assert_eq!(out[1]["region"], "intl");

        assert!(
            out[2].get("creditMultiplier").is_none(),
            "★ 无倍率概念时 key 必须**缺席** —— 下发 null 会显示成「不知道」"
        );
        assert!(
            out[2].get("region").is_none(),
            "★ 不分区产品不下发 region（空串是内部表示，不该外泄）"
        );
        assert!(out[2].get("regions").is_none());
        assert_eq!(out[2]["label"], "ZCode");
    }

    // -- product_models_from ---------------------------------------------------

    #[test]
    fn product_models_from_sorts_by_product_and_trims() {
        let mut products: HashMap<String, Vec<String>> = HashMap::new();
        // ★ 故意用逆序的键：不排序的话输出顺序随 HashMap 随机化，客户端模型
        // 菜单会出现「同一个模型在列表里来回跳」。
        products.insert(
            "zcode".to_string(),
            vec!["  b  ".to_string(), String::new(), "a".to_string()],
        );
        products.insert("qoder".to_string(), vec!["x".to_string()]);

        let out = product_models_from(&products);
        let pairs: Vec<(&str, &str)> = out
            .iter()
            .map(|p| (p.product.as_str(), p.id.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![("qoder", "x"), ("zcode", "b"), ("zcode", "a")],
            "★ 按产品名排序保证可复现；id 逐个 trim 且空串跳过"
        );
    }

    // -- merged_model_list：静态表只补真值空的一侧 ------------------------------

    #[test]
    fn merged_list_static_side_only_filled_when_region_has_no_truth() {
        // 国服有真值，国际版没有。
        let mut fetched: HashMap<Region, Option<Vec<ModelInfo>>> = HashMap::new();
        fetched.insert(Region::CN, Some(vec![mi("glm-5.3")]));
        fetched.insert(Region::Intl, None);
        let mut has = HashMap::new();
        has.insert(Region::CN, true);
        has.insert(Region::Intl, false);

        let list = CapabilityIndex::merged_model_list(&fetched, &has, &[], |_| Vec::new());

        assert!(has_entry(&list, "glm-5.3"), "真值里的模型必须在");
        assert!(
            !has_entry(&list, "hy4-preview"),
            "★ 国服有真值 ⇒ 国服静态表不参与宣称；hy4-preview 只在国服静态表里，\
             国际版真值里没有它 ⇒ 绝不能出现（所有者 2026-09-21 的明确要求）"
        );
        assert!(
            has_entry(&list, "hy4-preview-f"),
            "★ 国际版**没有**真值 ⇒ 国际版静态表兜底；hy4-preview-f 是另一个模型，必须在"
        );
    }

    #[test]
    fn merged_list_static_entries_keep_their_own_owned_by() {
        // 两区都拉不到 ⇒ 两侧都走静态兜底。
        let mut fetched: HashMap<Region, Option<Vec<ModelInfo>>> = HashMap::new();
        fetched.insert(Region::CN, None);
        fetched.insert(Region::Intl, None);
        let list = CapabilityIndex::merged_model_list(
            &fetched,
            &HashMap::new(),
            &[],
            |_| Vec::new(),
        );

        assert_eq!(entry(&list, "auto")["owned_by"], "workbuddy");
        assert_eq!(
            entry(&list, "default-model")["owned_by"],
            "workbuddy-intl",
            "★ owned_by 按各自那张表来，不能被国服的值覆盖"
        );
        // 静态表的 context_length 必须保留（整条 map 保留，不是只留 id）。
        assert_eq!(entry(&list, "auto")["context_length"], 256000);
    }

    #[test]
    fn merged_list_static_fallback_fabricates_no_efforts_but_keeps_image_capability() {
        let mut fetched: HashMap<Region, Option<Vec<ModelInfo>>> = HashMap::new();
        fetched.insert(Region::CN, None);
        fetched.insert(Region::Intl, None);
        let list =
            CapabilityIndex::merged_model_list(&fetched, &HashMap::new(), &[], |_| Vec::new());
        let e = entry(&list, "glm-5.3");

        assert_eq!(
            e["supportsImages"], true,
            "★ 静态表只声称「支持图片」（手抄表的已知信息），其余一律不编"
        );
        assert!(
            e.get("supported_efforts").is_none() && e.get("reasoning").is_none(),
            "★ 静态条目不带 efforts ⇒ 情形 C ⇒ 一个思考档位字段都不该下发；\
             凭空发一组档位会让客户端以为这些档位都被支持"
        );
    }

    #[test]
    fn merged_list_static_fallback_channel_has_multiplier_concept_but_null_value() {
        let mut fetched: HashMap<Region, Option<Vec<ModelInfo>>> = HashMap::new();
        fetched.insert(Region::CN, None);
        fetched.insert(Region::Intl, None);
        let list =
            CapabilityIndex::merged_model_list(&fetched, &HashMap::new(), &[], |_| Vec::new());

        let ch = channels_of(entry(&list, "glm-5.3"));
        assert_eq!(ch.len(), 2, "两区静态兜底 ⇒ 两条渠道");
        for c in ch {
            assert!(
                c.get("creditMultiplier").is_some(),
                "★ WorkBuddy **有**倍率概念，静态兜底期间只是没取到值 ⇒ 必须下发 null，\
                 而不是让整列消失（后者会让人以为这个平台不计费）"
            );
            assert!(c["creditMultiplier"].is_null());
            assert_eq!(c["product"], "workbuddy");
        }
    }

    // -- merged_model_list：动态真值优先 ---------------------------------------

    #[test]
    fn merged_list_prefers_cn_truth_for_shared_ids() {
        let mut cn = mi("glm-5.3");
        cn.context_window = 111;
        let mut intl = mi("glm-5.3");
        intl.context_window = 222;

        let list = CapabilityIndex::merged_model_list(
            &fetched_both(vec![cn], vec![intl]),
            &has_both(),
            &[],
            |_| Vec::new(),
        );

        assert_eq!(
            entry(&list, "glm-5.3")["context_length"],
            111,
            "★ 两区都有 ⇒ 国服赢（与图片请求优先走国服的既定策略一致）"
        );
        let ids: Vec<&str> = list
            .iter()
            .filter_map(|e| e.get("id").and_then(Value::as_str))
            .filter(|id| *id == "glm-5.3")
            .collect();
        assert_eq!(ids.len(), 1, "★ 裸名只出现一次（不是每区一条）");
    }

    #[test]
    fn merged_list_falls_back_to_131072_and_forces_qwen_to_1m() {
        let cn = vec![mi("unknown-model-x")]; // context_window = 0
        let products = vec![ProductModel {
            id: "Qwen3.8-Flash".to_string(),
            product: "qoder".to_string(),
        }];

        let list = CapabilityIndex::merged_model_list(
            &fetched_both(cn, Vec::new()),
            &has_both(),
            &products,
            |_| Vec::new(),
        );

        assert_eq!(
            entry(&list, "unknown-model-x")["context_length"],
            131072,
            "上游没给 context_window ⇒ 通用兜底 128K"
        );
        assert_eq!(
            entry(&list, "Qwen3.8-Flash")["context_length"],
            1000000,
            "★ Qoder 官方客户端把 Qwen3.8-Flash 配成 1M；用 128K 会让客户端提前压缩、\
             无谓地丢上下文"
        );
    }

    // -- merged_model_list：组合名独立条目 + 渠道过滤 ---------------------------

    #[test]
    fn merged_list_alias_entries_are_standalone_and_filtered() {
        let products = vec![ProductModel {
            id: "glm-5.3".to_string(),
            product: "zcode".to_string(),
        }];

        let list = CapabilityIndex::merged_model_list(
            &fetched_both(vec![mi("glm-5.3")], vec![mi("glm-5.3")]),
            &has_both(),
            &products,
            |_| Vec::new(), // zcode 账号覆及的区域为空 ⇒ 只发 `zcode:模型名`
        );

        let bare = entry(&list, "glm-5.3");
        let aliases = bare["aliases"]
            .as_array()
            .expect("裸名条目该带 aliases")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        assert!(aliases.contains(&"zcode:glm-5.3"));
        assert!(aliases.contains(&"workbuddy:glm-5.3"));
        assert!(aliases.contains(&"国服:glm-5.3"));
        assert!(aliases.contains(&"workbuddy:国服:glm-5.3"));

        // ★ 组合名必须是**独立条目**：很多 OpenAI 客户端只读 `data[].id`，
        // 只挂 `aliases` 的话用户在模型选择器里根本看不到组合名。
        let alias_entry = entry(&list, "zcode:glm-5.3");
        assert_eq!(alias_entry["owned_by"], "gateway-route");
        assert_eq!(alias_entry["route_for"], "glm-5.3");
        assert!(
            alias_entry.get("aliases").is_none(),
            "★ 组合名条目不该再带 aliases（会递归展示成一片）"
        );
        assert_eq!(
            alias_entry["context_length"],
            1000000,
            "★ 组合名条目继承裸名的元数据；裸名的 context_length 来自静态表\
             （glm-5.3 在国服表里是 1M，动态真值没给 context_window 时不覆盖）"
        );
        assert_eq!(alias_entry["object"], "model");

        // 渠道按产品过滤：`zcode:...` 只该显示 zcode 那条。
        let ch = channels_of(alias_entry);
        assert_eq!(ch.len(), 1, "★ 明确指定了平台，就不该再显示其它平台的渠道");
        assert_eq!(ch[0]["product"], "zcode");
        assert_eq!(ch[0]["label"], "ZCode");
        assert!(
            ch[0].get("creditMultiplier").is_none(),
            "★ ZCode 没有倍率数据源 ⇒ has_multiplier 留 false ⇒ key 缺席（宁可不显示也不编）"
        );
    }

    #[test]
    fn merged_list_alias_region_filter_keeps_only_that_region() {
        let list = CapabilityIndex::merged_model_list(
            &fetched_both(vec![mi("glm-5.3")], vec![mi("glm-5.3")]),
            &has_both(),
            &[],
            |_| Vec::new(),
        );

        let cn_alias = channels_of(entry(&list, "国服:glm-5.3"));
        assert_eq!(cn_alias.len(), 1, "★ 组合名指定了区域 ⇒ 只留该区域渠道");
        assert_eq!(cn_alias[0]["region"], "cn");

        let intl_alias = channels_of(entry(&list, "workbuddy:国际版:glm-5.3"));
        assert_eq!(intl_alias.len(), 1);
        assert_eq!(intl_alias[0]["region"], "intl");

        // 裸名条目相反：两区渠道都要展示（用户没指定区域，得看到两条）。
        assert_eq!(channels_of(entry(&list, "glm-5.3")).len(), 2);
    }

    #[test]
    fn merged_list_alias_channels_filter_matches_resolve_model_semantics() {
        // 组合名是「用户能写、且我们自己读得回来」的承诺 —— 这里直接对着
        // resolve_model 断言，避免下发一个解析不出平台的写法。
        let products = vec![ProductModel {
            id: "glm-5.3".to_string(),
            product: "qoder".to_string(),
        }];
        let list = CapabilityIndex::merged_model_list(
            &fetched_both(vec![mi("glm-5.3")], Vec::new()),
            &has_both(),
            &products,
            |_| vec![Region::CN],
        );

        // product_regions 返回国服 ⇒ 该发 `qoder:国服:glm-5.3`。
        let alias = "qoder:国服:glm-5.3";
        assert!(
            has_entry(&list, alias),
            "★ 有该区域账号时，必须给出「平台+区域+模型」三段写法"
        );

        let r = resolve_model(alias);
        assert_eq!(r.product, "qoder");
        assert_eq!(r.realm, crate::resolve_model::REALM_CN);
        assert_eq!(r.bare, "glm-5.3");
        assert_eq!(realm_to_region(&r.realm), Region::CN);

        // ⚠ 这里是 Go 原版的**真实行为**，不是我的实现缺陷 —— 逐字对应
        // `capability.go:1167-1169`：`if wantRegion != "" && r != wantRegion { continue }`。
        //
        // 三段写法要求 `region == "cn"`，而 Qoder 渠道的 `region` 是**空串**
        //（Qoder 渠道由 `productModels` 追加，不带区域 —— 见 `:1201-1202`
        // 注释「无区域的渠道（如 Qoder/ZCode 这类不分区…）用 Region == "" 表示」）。
        // 空串 != "cn" ⇒ **每一条渠道都被过滤掉**，结果是一个空数组。
        //
        // ★ 可达性已确认：Qoder 账号**确实**带区域（`go-gateway\internal\auth\
        // region_realcreds_test.go:25-26`：`qoder.sh` → Intl、`qoder.com.cn` → CN），
        // 所以 `product_regions("qoder")` 非空 ⇒ 三段别名真的会被生成。
        //
        // 用户可见后果：`qoder:国服:xxx` 这个模型项没有渠道信息（界面显示不出平台）。
        // 路由本身不受影响（`resolve_model` 仍解析出 product=qoder）。
        //
        // 已作为**观察**上报 lead，由他决定是否在 Go 侧先修（两侧必须保持一致，
        // 不能只有 Rust 改）。本测试钉住当前行为，防止我"顺手改好"造成两侧分叉。
        let ch = channels_of(entry(&list, alias));
        assert!(
            ch.is_empty(),
            "★ 忠实移植 Go：区域限定别名会滤掉不分区渠道（见上方注释）；实际 = {ch:?}"
        );

        // 对照：**不带**区域的三段写法 `qoder:glm-5.3` 的 wantRegion 为空 ⇒
        // 只按 product 过滤 ⇒ 保留那条不分区渠道。这才是用户该用的写法。
        let two_part = channels_of(entry(&list, "qoder:glm-5.3"));
        assert_eq!(two_part.len(), 1);
        assert_eq!(two_part[0]["product"], "qoder");
        assert_eq!(two_part[0]["label"], "Qoder");
        assert!(
            two_part[0].get("region").is_none(),
            "不分区渠道不下发 region"
        );
    }
}
