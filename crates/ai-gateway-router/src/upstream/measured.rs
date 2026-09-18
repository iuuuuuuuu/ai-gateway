//! 逐账号**实测**得到的图片能力结论，用于覆盖上游元数据。
//!
//! 对应 Go 源文件 `internal/server/measured.go`。
//!
//! # 为什么需要「覆盖上游声明」
//!
//! 上游 `/v3/config` 对每个模型都给了 `supportsImages`，但实测发现它对
//! **两区同名、后端不同**的模型给出的是**同一份（错误）**答案：
//!
//! ```text
//! glm-5.3 / glm-5.2   两区都报 supportsImages=true
//!   国服后端   真的能读图（64x64 红图 → prompt_tokens +22，答「红色」）
//!   国际版后端 读不到图（同样的图 → 增量恒 +33，答「我无法查看图片」）
//! ```
//!
//! 直接信上游的后果：客户端在本地就认定「支持图片」，用户拖图进去，请求被路由到
//! 国际版后端，图片被**静默**替换成占位符，模型回一句「抱歉，我无法查看图片」——
//! 用户以为模型不行，实际是元数据撒了谎。
//!
//! 因此这里把实测结论显式记下来，作为比上游声明**更高优先级**的真值来源。
//!
//! # 判据（怎么算「实测确认不支持」）
//!
//! 发一张**纯色小图**，问「这是什么颜色」，同时对比纯文本请求的 `prompt_tokens`：
//!
//! - 图片被真正解码 → token 增量随图片体积变化，且能答对颜色
//! - 图片被换占位符 → 增量是**常数**（与图片体积无关），且答「无法查看图片」
//!
//! 常数增量是关键：把 64x64 红图换成 512x512 蓝图，增量**完全相同**，
//! 这不可能是解码结果，只能是固定长度的占位符。
//!
//! # 实测记录（2026-09-16）
//!
//! 2 个国服 + 5 个国际版账号，模型 × {64x64 纯红, 512x512 纯蓝}：
//!
//! | 模型      | 区域   | 文本pt | 小图pt | 大图pt | 小图Δ | 大图Δ | 回答         |
//! |-----------|--------|--------|--------|--------|-------|-------|--------------|
//! | glm-5.3   | 国服   | 26     | 48     | 393    | +22   | +367  | 红色/蓝色 ✅ |
//! | glm-5.3   | 国际版 | 26     | 59     | 59     | +33   | +33   | 无法查看 ❌  |
//! | glm-5.2   | 国服   | 20     | 48     | 393    | +28   | +373  | 红色/蓝色 ✅ |
//! | glm-5.2   | 国际版 | 20     | 53     | 53     | +33   | +33   | 无法查看 ❌  |
//! | hy3       | 国服   | 25     | 47     | 205    | +22   | +180  | 红色/蓝色 ✅ |
//! | hy3       | 国际版 | 25     | 184    | 184    | +159  | +159  | 红色 ⚠️      |
//! | kimi-k2.6 | 国服   | 23     | 40     | 40     | +17   | +17   | 红色 ⚠️      |
//! | kimi-k2.6 | 国际版 | 23     | 40     | 40     | +17   | +17   | 颜色 ⚠️      |
//!
//! 结论分三类，**不能一概而论**：
//!
//! - `glm-5.2` / `glm-5.3`：国服能读、国际版不能 → 需要区域强制 + 覆盖国际版声明
//! - `hy3`：两区都能读（编码不同）→ 不强制区域，也不覆盖
//! - `kimi-k2.6`：两区都能读（增量同为常数 17，但答对了颜色）
//!
//! kimi 那行是刻意留着的**反例**：它的增量也是常数，但回答正确。只看「增量是否常数」
//! 会把 kimi 误判成不支持 —— 所以判据必须是「增量常数 **且** 回答否认看到图片」，
//! 两者同时成立才算不支持。

use crate::pool::Region;

/// 实测得到的图片能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageCapability {
    /// 未实测：以模型为单位，语义是「没有 conflicting 的实测结论」。
    ///
    /// 与「实测支持」不同：实测支持是「验过，确实能读」；未实测只是
    /// 「没验出问题」—— 对上游声明不做任何覆盖，保持原样透传。
    Unmeasured,
    /// 实测确认能读图。
    Supported,
    /// 实测确认读不到图（图片被替换成固定占位符）。
    Unsupported,
}

/// 返回某模型在指定区域的**实测**图片能力。
///
/// `region` 为 [`Region::Any`] 时返回「各区域实测结论的交集」：
///
/// - 两区都实测支持 → `Supported`
/// - 两区都实测不支持 → `Unsupported`
/// - 一支持一不支持 → `Unsupported`（**保守**：网关无法保证用户会被路由到能读图的
///   那一侧，谎报 true 会让客户端发出必然静默降级的请求）
/// - 任一区未实测 → `Unmeasured`（不覆盖）
///
/// 保守优先（conflicting 取 false）是刻意的：宁可让客户端在混合池下少用一点图片
/// 能力，也不要让它发出一个「看起来成功、实际图片被丢掉」的请求 ——
/// 后者用户完全无感知，只表现为模型「莫名其妙说看不见图片」。
pub fn measured_image_capability(model: &str, region: Region) -> ImageCapability {
    let m = model.trim().to_ascii_lowercase();
    if m.is_empty() {
        return ImageCapability::Unmeasured;
    }
    let cn = measured_for_region(&m, Region::CN);
    let intl = measured_for_region(&m, Region::Intl);

    if region == Region::Any {
        return match (cn, intl) {
            (ImageCapability::Unmeasured, _) | (_, ImageCapability::Unmeasured) => {
                ImageCapability::Unmeasured
            }
            (ImageCapability::Supported, ImageCapability::Supported) => ImageCapability::Supported,
            _ => ImageCapability::Unsupported,
        };
    }
    measured_for_region(&m, region)
}

/// 单个模型的区域实测结论。
///
/// 用 `match` 而不是表：条目很少（目前只有 glm-5.2/5.3 两区各不相同），
/// `match` 让每条结论与它的证据注释写在**一起**，读代码时不用跳来跳去查表。
/// 表大了再换 `HashMap`。
fn measured_for_region(model: &str, region: Region) -> ImageCapability {
    match model {
        // 国服能读、国际版读不到（增量恒 +33，与图片体积无关，答「无法查看图片」）。
        "glm-5.3" | "glm-5.2" => {
            if region == Region::CN {
                ImageCapability::Supported
            } else {
                ImageCapability::Unsupported
            }
        }
        _ => ImageCapability::Unmeasured,
    }
}

/// 按实测结论覆盖上游声明的图片能力。
///
/// - 实测不支持 → 覆盖成 `Some(false)`（**不管上游说什么**）。这是本函数存在的
///   全部意义：上游对国际版 glm-5.x 报 true，实测是 false，必须以实测为准。
/// - 实测支持 → 覆盖成 `Some(true)`（上游漏标时也能纠正）。
/// - 未实测 → 原样返回上游声明（三态：`None` 仍是 `None`）。
pub fn measured_override(model: &str, region: Region, upstream: Option<bool>) -> Option<bool> {
    match measured_image_capability(model, region) {
        ImageCapability::Unsupported => Some(false),
        ImageCapability::Supported => Some(true),
        ImageCapability::Unmeasured => upstream,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glm_is_region_split() {
        // 国服能读、国际版读不到 —— 这是本表存在的唯一理由
        assert_eq!(
            measured_image_capability("glm-5.3", Region::CN),
            ImageCapability::Supported
        );
        assert_eq!(
            measured_image_capability("glm-5.3", Region::Intl),
            ImageCapability::Unsupported
        );
        assert_eq!(
            measured_image_capability("glm-5.2", Region::CN),
            ImageCapability::Supported
        );
        assert_eq!(
            measured_image_capability("glm-5.2", Region::Intl),
            ImageCapability::Unsupported
        );
    }

    /// 大小写与首尾空白不敏感。
    #[test]
    fn model_name_normalized() {
        assert_eq!(
            measured_image_capability("  GLM-5.3  ", Region::CN),
            ImageCapability::Supported
        );
    }

    /// 未实测的模型不得被覆盖（「没验过」≠「不行」）。
    #[test]
    fn unmeasured_models_are_not_overridden() {
        for m in ["hy3", "kimi-k2.6", "gpt-5.6", "unknown"] {
            assert_eq!(
                measured_image_capability(m, Region::CN),
                ImageCapability::Unmeasured,
                "{m}"
            );
            // 三态保持：上游说 true / false / 未声明，一律原样透传
            assert_eq!(measured_override(m, Region::CN, Some(true)), Some(true));
            assert_eq!(measured_override(m, Region::CN, Some(false)), Some(false));
            assert_eq!(measured_override(m, Region::CN, None), None);
        }
    }

    /// RegionAny 取交集：conflicting 时保守取 false。
    #[test]
    fn any_region_is_conservative_on_conflict() {
        // glm 两区冲突 → 不支持（谎报 true 会让请求静默降级）
        assert_eq!(
            measured_image_capability("glm-5.3", Region::Any),
            ImageCapability::Unsupported
        );
        // 未实测模型 → 未实测（不覆盖）
        assert_eq!(
            measured_image_capability("hy3", Region::Any),
            ImageCapability::Unmeasured
        );
        // 空模型名 → 未实测
        assert_eq!(
            measured_image_capability("", Region::Any),
            ImageCapability::Unmeasured
        );
    }

    /// 实测不支持时**必须**覆盖上游的 true —— 这是本模块存在的意义。
    #[test]
    fn override_beats_upstream_claim() {
        // 上游对国际版 glm-5.3 报 supportsImages=true，实测是 false
        assert_eq!(
            measured_override("glm-5.3", Region::Intl, Some(true)),
            Some(false),
            "实测必须压过上游的错误声明"
        );
        // 实测支持时纠正上游的漏标
        assert_eq!(
            measured_override("glm-5.3", Region::CN, Some(false)),
            Some(true)
        );
    }
}
