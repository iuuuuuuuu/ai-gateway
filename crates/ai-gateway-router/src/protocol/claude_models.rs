//! Claude 模型名 → 上游真实模型名的映射。
//!
//! 对应 Go 源文件 `internal/server/messages.go` 的映射区。
//!
//! # 为什么需要这层映射
//!
//! Claude 客户端只会请求 `claude-*` 名字（内置型号或配置里的虚拟名），而上游
//! 只有 `deepseek-v4-flash` 这类真实名。不翻译就直接 11102
//!（`model service info not found`）。
//!
//! # 两级映射
//!
//! - **aliases**：精确名映射。来自虚拟名/真实名成对配置（CC Switch 风格）以及
//!   Claude Desktop profile 的 `name → labelOverride`。
//! - **slots**：槽位兜底（`sonnet` / `opus` / `haiku` / `fable` → 真实名）。
//!   Claude 客户端的 `/model` 菜单里可以直接选中内置型号（如 `claude-opus-5`），
//!   这类名字不经过客户端配置的四个槽位，精确表命中不了，需要按名字中的槽位
//!   关键词回退，否则用户在菜单里换个型号就会撞上 11102。
//!
//! # 失败语义是刻意设计的
//!
//! 映射表读不到时**原样透传**，宁可让上游报 11102（可定位），
//! 也不要静默替换成用户没选的模型。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 映射表缓存 TTL。
///
/// 为什么要缓存：映射来自客户端配置文件（`~/.claude/settings.json` 与
/// Claude Desktop 的 configLibrary），每次 `/v1/messages` 都要读两处磁盘
///（其中一处还是目录扫描）代价过高。配置变更不频繁，30 秒足够新鲜。
const ALIAS_TTL: Duration = Duration::from_secs(30);

/// 两级映射表。
#[derive(Debug, Clone, Default)]
pub struct ClaudeModelTables {
    /// 精确名（小写）→ 真实名。
    pub aliases: HashMap<String, String>,
    /// 槽位名（小写）→ 真实名。
    pub slots: HashMap<String, String>,
}

impl ClaudeModelTables {
    /// 按名字中的槽位关键词取对应槽位的模型；无关键词时回退主槽位 `sonnet`。
    fn slot_fallback(&self, lower: &str) -> Option<&String> {
        for slot in ["opus", "sonnet", "haiku", "fable"] {
            if lower.contains(slot) {
                if let Some(v) = self.slots.get(slot) {
                    if !v.is_empty() {
                        return Some(v);
                    }
                }
            }
        }
        self.slots.get("sonnet").filter(|v| !v.is_empty())
    }
}

/// 缓存：`(表, 抓取时刻)`。
fn cache() -> &'static Mutex<Option<(ClaudeModelTables, Instant)>> {
    static CACHE: OnceLock<Mutex<Option<(ClaudeModelTables, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 把客户端传入的 Claude 模型名翻译成上游真实模型名。
///
/// 翻译顺序：精确别名 → 槽位关键词兜底 → 原样透传。
pub fn resolve_claude_model(requested: &str) -> String {
    let name = requested.trim().to_string();
    let lower = name.to_lowercase();
    if !lower.starts_with("claude-") {
        return name;
    }
    let tables = cached_tables();
    if let Some(mapped) = tables.aliases.get(&lower) {
        if !mapped.is_empty() {
            return mapped.clone();
        }
    }
    if let Some(mapped) = tables.slot_fallback(&lower) {
        return mapped.clone();
    }
    name
}

/// 取缓存的映射表；过期或未加载时重建。
fn cached_tables() -> ClaudeModelTables {
    {
        let guard = cache().lock().unwrap_or_else(|p| p.into_inner());
        if let Some((t, at)) = guard.as_ref() {
            if at.elapsed() < ALIAS_TTL {
                return t.clone();
            }
        }
    }
    let tables = load_tables();
    let mut guard = cache().lock().unwrap_or_else(|p| p.into_inner());
    *guard = Some((tables.clone(), Instant::now()));
    tables
}

/// 清空缓存（供测试隔离；生产路径不调用）。
pub fn reset_cache() {
    let mut guard = cache().lock().unwrap_or_else(|p| p.into_inner());
    *guard = None;
}

/// 从两处客户端配置汇总映射（失败返回空表，**不报错**）。
fn load_tables() -> ClaudeModelTables {
    let mut t = ClaudeModelTables::default();
    load_claude_code_tables(&mut t);
    load_claude_desktop_tables(&mut t);
    t
}

/// 读 `~/.claude/settings.json` 的 `env` 块。
///
/// 兼容两种配置形态：
///
/// ```text
/// 直写真实名（本应用）：ANTHROPIC_DEFAULT_SONNET_MODEL = deepseek-v4-flash
/// 虚拟名映射（CC Switch 等）：_MODEL = claude-sonnet-4-6，_MODEL_NAME = deepseek-v4-flash
/// ```
fn load_claude_code_tables(t: &mut ClaudeModelTables) {
    let Some(dir) = claude_config_dir() else {
        return;
    };
    let Ok(data) = std::fs::read_to_string(dir.join("settings.json")) else {
        return;
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(&data) else {
        return;
    };
    let Some(env) = root.get("env").and_then(|e| e.as_object()) else {
        return;
    };
    let get = |k: &str| -> String {
        env.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };

    for slot in ["SONNET", "OPUS", "HAIKU", "FABLE"] {
        let model = get(&format!("ANTHROPIC_DEFAULT_{slot}_MODEL"));
        let name = get(&format!("ANTHROPIC_DEFAULT_{slot}_MODEL_NAME"));
        if !model.is_empty() && !name.is_empty() {
            t.aliases.insert(model.to_lowercase(), name.clone());
        }
        // 槽位兜底优先取显示名（虚拟名形态下它才是真实名），其次取模型名；
        // `claude-` 前缀的值只是客户端的虚拟名，不能作为兜底目标。
        let mut candidate = name;
        if candidate.is_empty() || candidate.to_lowercase().starts_with("claude-") {
            candidate = model;
        }
        if !candidate.is_empty() && !candidate.to_lowercase().starts_with("claude-") {
            t.slots.insert(slot.to_lowercase(), candidate);
        }
    }
    // ANTHROPIC_MODEL 只作兜底：它可能本身还是虚拟名（循环映射无意义）。
    let m = get("ANTHROPIC_MODEL");
    if !m.is_empty() && !m.to_lowercase().starts_with("claude-") {
        t.aliases.insert("claude-sonnet-4-6".into(), m.clone());
        t.slots.entry("sonnet".into()).or_insert(m);
    }
}

/// Claude 配置目录（尊重 `CLAUDE_CONFIG_DIR`）。
fn claude_config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir().map(|h| h.join(".claude"))
}

/// 读 Claude Desktop 的 3P profile。
///
/// profile 的 `inferenceModels` 里，`name` 是 Claude 槽位名、
/// `labelOverride` 才是真实上游模型。
fn load_claude_desktop_tables(t: &mut ClaudeModelTables) {
    let Some(local) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let lib_dir = PathBuf::from(local)
        .join("Claude-3p")
        .join("configLibrary");
    let Ok(entries) = std::fs::read_dir(&lib_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".json") || name == "_meta.json" {
            continue;
        }
        let Ok(data) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(prof) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        let Some(models) = prof.get("inferenceModels").and_then(|m| m.as_array()) else {
            continue;
        };
        for im in models {
            let im_name = im.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let label = im.get("labelOverride").and_then(|v| v.as_str()).unwrap_or("");
            if im_name.is_empty() || label.is_empty() {
                continue;
            }
            let lower = im_name.to_lowercase();
            t.aliases.insert(lower.clone(), label.to_string());
            for slot in ["sonnet", "opus", "haiku", "fable"] {
                if lower.contains(slot) {
                    t.slots.insert(slot.into(), label.to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables(aliases: &[(&str, &str)], slots: &[(&str, &str)]) -> ClaudeModelTables {
        ClaudeModelTables {
            aliases: aliases
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            slots: slots
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    /// 非 claude- 前缀的名字原样透传（上游真实名不该被翻译）。
    #[test]
    fn non_claude_names_pass_through() {
        assert_eq!(resolve_claude_model("deepseek-v4-flash"), "deepseek-v4-flash");
        assert_eq!(resolve_claude_model("gpt-5.6"), "gpt-5.6");
        assert_eq!(resolve_claude_model(""), "");
    }

    /// 精确别名优先于槽位兜底。
    #[test]
    fn alias_beats_slot_fallback() {
        let t = tables(
            &[("claude-sonnet-4-6", "deepseek-v4-flash")],
            &[("sonnet", "glm-5.3")],
        );
        assert_eq!(t.aliases["claude-sonnet-4-6"], "deepseek-v4-flash");
        assert_eq!(
            t.slot_fallback("claude-sonnet-4-6").map(String::as_str),
            Some("glm-5.3"),
            "槽位兜底是另一条路径"
        );
    }

    /// 槽位关键词匹配：`/model` 菜单里选内置型号（精确表命中不了）也能落到槽位。
    #[test]
    fn slot_fallback_matches_keyword() {
        let t = tables(
            &[],
            &[
                ("opus", "glm-5.3"),
                ("sonnet", "deepseek-v4-flash"),
                ("haiku", "deepseek-v4-flash"),
            ],
        );
        assert_eq!(
            t.slot_fallback("claude-opus-5").map(String::as_str),
            Some("glm-5.3")
        );
        assert_eq!(
            t.slot_fallback("claude-3-5-haiku-latest").map(String::as_str),
            Some("deepseek-v4-flash")
        );
        // 无关键词 → 回退主槽位 sonnet
        assert_eq!(
            t.slot_fallback("claude-unknown-thing").map(String::as_str),
            Some("deepseek-v4-flash")
        );
    }

    /// 空槽位不得被当作有效映射（否则会把名字替换成空串）。
    #[test]
    fn empty_slot_is_not_a_mapping() {
        let t = tables(&[], &[("sonnet", "")]);
        assert!(t.slot_fallback("claude-anything").is_none());
    }

    /// 映射表读不到时原样透传 —— 宁可让上游报 11102（可定位），
    /// 也不要静默替换成用户没选的模型。
    #[test]
    fn empty_tables_pass_through() {
        let t = ClaudeModelTables::default();
        assert!(t.aliases.is_empty());
        assert!(t.slot_fallback("claude-opus-5").is_none());
    }
}
