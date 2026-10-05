//! 「限制使用的模型」白名单（多选）。
//!
//! 对应 Go 源文件 `internal/server/model_allow.go`（**全文仅 80 行**）。
//!
//! # 语义演进
//!
//! 此前是**单值** `AllowedModel`，且仅在「单一模型 + 积分轮转」模式下生效；
//! 现在升级为**多值白名单 + 三个工作模式都有**，默认（空）不限制。
//!
//! # 三条硬约束，任何一条破了都是**破坏性变更**
//!
//! 1. **空 = 不限制**。老配置升级后行为必须逐字不变 —— 老配置里根本可能没有这个
//!    键，把它读成「一个模型都不放行」会让网关对所有请求返回 400，且用户完全无从
//!    判断是自己配错了还是新版本坏了。
//! 2. **大小写不敏感**。客户端写模型名的大小写并不统一（实测同时出现过
//!    `DeepSeek-V4.1-Flash` 与 `deepseek-v4.1-flash`），逐字节比较会误拒。
//! 3. **比较前剥掉区域前缀** `cn:` / `global:`。前缀只是给网关的**选号指令**，
//!    上游不认识它，转发前也会被 [`crate::forward::rewrite_model`] 抹掉；因此
//!    「配的是裸名、客户端带前缀请求」必须视为同一个模型。

use crate::resolve_model::resolve_model;

/// 归一化白名单：去空白、剥前缀、丢弃空项。
///
/// 刻意**不去重**：列表只有几个元素，去重带来的复杂度（且要额外分配）换不来
/// 任何可观测收益；重复项在匹配时只是多一次比较。
///
/// 刻意**不做大小写折叠**：折叠要额外分配一份新 `String`，而比较侧用
/// [`str::eq_ignore_ascii_case`] 已经是零分配的 —— 归一化只负责「形状」，
/// 大小写交给比较。
///
/// # ⚠ 必须能剥**产品前缀**
///
/// `qoder:glm-5.3` 这种带产品前缀的名字也要归一成裸名 —— 否则用户在界面上
/// 复制一个带前缀的完整名填进白名单，那个模型就**永远匹配不上**，表现为
/// 「我明明加了它，却报不在放行清单里」。
pub fn normalize_allowed_models(raw: &[String]) -> Vec<String> {
    if raw.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for item in raw {
        // 与请求侧同样先剥前缀：用户既可能在界面上选到裸名，也可能手写带前缀的名。
        // `resolve_model` 的返回是具名字段 `(product, realm, bare)`，取错字段会把
        // 前缀当成模型名 —— Go 侧正是为此踩过坑（见 `resolve_model.rs` 的文档）。
        let bare = resolve_model(item.trim()).bare;
        let bare = bare.trim();
        if !bare.is_empty() {
            out.push(bare.to_string());
        }
    }
    if out.is_empty() {
        return Vec::new();
    }
    out
}

/// 判断请求的**裸模型名**是否在白名单内；白名单为空 = 全部放行。
///
/// 调用方必须传剥过前缀的裸名（[`crate::server::run_forward`] 里已是如此）——
/// 这里再兜一次 [`resolve_model`]，是为了让「有人直接从别处调这个函数」也不会
/// 因带前缀而误拒。
///
/// # ⚠ 这一次兜底同时覆盖**产品前缀与区域前缀**
///
/// `qoder:glm-5.3`、`zcode:国际版:glm-5.3` 都必须与裸名 `glm-5.3` 视为同一个
/// 模型。否则用户在客户端用前缀指定平台时，会被白名单判成「不在放行清单」——
/// 而他明明放行了那个模型（这是最难自查的一类误拒）。
pub fn model_allowed(allowed: &[String], model: &str) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let bare = resolve_model(model).bare;
    allowed.iter().any(|item| bare.eq_ignore_ascii_case(item))
}

/// 把白名单拼成给用户看的一行文案（顿号分隔）。
///
/// 错误信息必须**列出全部**允许的模型：只报「不允许 x」在用户配了 3 个模型时
/// 完全没法排查 —— 他不知道该改成哪一个，只能挨个试。
pub fn allowed_models_text(allowed: &[String]) -> String {
    allowed.join("、")
}

/// 「模型不在放行清单」的面向用户文案。
///
/// 对应 Go `forward.go:99-109` 的 `(*modelLockedError).Error()`，逐字对齐。
///
/// 为什么在文案里**同时**给出「收到的是哪个」与「允许哪些」：用户看到 400 的第一
/// 反应是「我改了什么」还是「客户端发了什么」，两个信息缺一个都得来回试。
pub fn model_locked_message(requested: &str, allowed: &[String]) -> String {
    let got = if requested.is_empty() {
        "(未指定)"
    } else {
        requested
    };
    let list = allowed_models_text(allowed);
    format!(
        "当前已限制可使用的模型，只允许调用 {list}；收到的是 {got}。\
请在客户端把模型改为 {list} 中的任意一个，或在界面的「放行模型」里调整限制。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// 空 = 不限制（硬约束 ①）。老配置没有这个键时行为必须逐字不变。
    #[test]
    fn empty_means_unrestricted() {
        assert!(model_allowed(&[], "anything"));
        assert!(model_allowed(&normalize_allowed_models(&[]), "anything"));
        // 全是空白/空串的名单归一化后也是空 ⇒ 不限制，而不是「一个都不放行」。
        let only_blank = normalize_allowed_models(&v(&["", "   "]));
        assert!(only_blank.is_empty());
        assert!(model_allowed(&only_blank, "anything"));
    }

    /// 大小写不敏感（硬约束 ②）。
    #[test]
    fn match_is_case_insensitive() {
        let a = normalize_allowed_models(&v(&["DeepSeek-V4.1-Flash"]));
        assert!(model_allowed(&a, "deepseek-v4.1-flash"));
        assert!(model_allowed(&a, "DEEPSEEK-V4.1-FLASH"));
        // 反向：配的是小写、请求是大写。
        let b = normalize_allowed_models(&v(&["glm-5.3"]));
        assert!(model_allowed(&b, "GLM-5.3"));
        // 不同名仍然拒绝。
        assert!(!model_allowed(&b, "glm-5.4"));
    }

    /// 区域前缀被剥掉（硬约束 ③）。
    #[test]
    fn realm_prefix_is_stripped_on_both_sides() {
        let a = normalize_allowed_models(&v(&["cn:glm-5.3", "global:kimi-k3"]));
        assert_eq!(a, v(&["glm-5.3", "kimi-k3"]));
        // 名单是裸名、请求带前缀 ⇒ 同一个模型。
        assert!(model_allowed(&a, "cn:glm-5.3"));
        assert!(model_allowed(&a, "global:glm-5.3"));
        // 名单带前缀、请求是裸名 ⇒ 也是同一个模型。
        assert!(model_allowed(&a, "glm-5.3"));
    }

    /// ★ 产品前缀同样必须剥 —— 否则用户从界面复制 `qoder:glm-5.3` 填进白名单后
    /// 永远匹配不上，症状是「我明明加了它，却报不在放行清单里」。
    #[test]
    fn product_prefix_is_stripped_too() {
        let a = normalize_allowed_models(&v(&["qoder:glm-5.3", "zcode:国际版:glm-5.3"]));
        assert_eq!(a, v(&["glm-5.3", "glm-5.3"]));
        // 请求带产品前缀（客户端指定平台）时，仍应命中裸名白名单。
        let bare = normalize_allowed_models(&v(&["glm-5.3"]));
        assert!(model_allowed(&bare, "qoder:glm-5.3"));
        assert!(model_allowed(&bare, "zcode:国际版:glm-5.3"));
        assert!(model_allowed(&bare, "qoder:global:glm-5.3"));
    }

    /// 元素两侧空白被忽略；空项被丢弃。
    #[test]
    fn items_are_trimmed_and_blanks_dropped() {
        let a = normalize_allowed_models(&v(&["  glm-5.3  ", "", "   ", " kimi-k3"]));
        assert_eq!(a, v(&["glm-5.3", "kimi-k3"]));
    }

    /// 刻意**不去重**（与 Go 一致）：重复项只是多一次比较，去重换不来可观测收益。
    #[test]
    fn duplicates_are_kept_deliberately() {
        let a = normalize_allowed_models(&v(&["glm-5.3", "GLM-5.3", "glm-5.3"]));
        assert_eq!(a.len(), 3);
        // 但匹配结果不受影响。
        assert!(model_allowed(&a, "glm-5.3"));
    }

    /// `deepseek:v3` 里的 `deepseek` 不是产品前缀 ⇒ 整串原样保留，不能被吃掉。
    #[test]
    fn non_prefix_colon_is_not_stripped() {
        let a = normalize_allowed_models(&v(&["deepseek:v3"]));
        assert_eq!(a, v(&["deepseek:v3"]));
        assert!(model_allowed(&a, "deepseek:v3"));
        assert!(!model_allowed(&a, "v3"));
    }

    /// 文案必须列出**全部**允许的模型（用户配了多个时才知道该改成哪个）。
    #[test]
    fn message_lists_all_allowed_models() {
        let a = normalize_allowed_models(&v(&["a", "b", "c"]));
        let msg = model_locked_message("x", &a);
        assert!(msg.contains("只允许调用 a、b、c"), "{msg}");
        assert!(msg.contains("收到的是 x"), "{msg}");
        assert!(msg.contains("「放行模型」"), "{msg}");
        // 请求体没带 model 时的占位文案（Go `got = "(未指定)"`）。
        assert!(model_locked_message("", &a).contains("收到的是 (未指定)"));
    }

    #[test]
    fn text_joins_with_ideographic_comma() {
        assert_eq!(allowed_models_text(&v(&["a", "b"])), "a、b");
        assert_eq!(allowed_models_text(&[]), "");
    }
}
