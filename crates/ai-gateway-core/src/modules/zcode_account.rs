// zcode_account.rs ZCode 账号库（宿主侧）。
//
// 与 qoder_account.rs 同构（同样的「账号元信息 + 凭证分离」设计），
// 但**多一个字段：服务商**（Z.AI / 智谱）—— 那不是"区域"，
// 而是两个不同的服务商（不同域名、不同凭证来源）。
//
// 目录布局：
//
//	~/.wb-switch/zcode/
//	├── accounts.json          账号元信息
//	└── auths/
//	    └── zcode-<uid>.json   ZCode 凭证（由 Go 侧 internal/zcode 读写）
//
// 与 Qoder 的差异：ZCode 的凭证是**用户可复制的字符串**，
// 故"导入"是主路径 —— 账号库要支持"只有凭证、没有元信息"的形态。

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::modules::config;

/// ZCode 账号库的根目录。
pub fn store_dir() -> PathBuf {
    config::store_dir().join("zcode")
}

/// 凭证目录（Go 侧的 auth_dir 指向这里）。
pub fn auth_dir() -> PathBuf {
    store_dir().join("auths")
}

fn accounts_path() -> PathBuf {
    store_dir().join("accounts.json")
}

/// 确保目录存在。
pub fn ensure_dirs() -> Result<(), String> {
    std::fs::create_dir_all(auth_dir()).map_err(|e| format!("创建 ZCode 目录失败: {e}"))
}

/// 单个账号的元信息。
///
/// 字段刻意保持精简：**凭证不在这里**（它们在 auths/ 下由 Go 侧管理），
/// 这里只存界面需要展示与筛选的东西。
#[derive(Debug, Clone, Default)]
pub struct ZcodeAccount {
    /// 账号标识（Go 侧派生的 `zcode-<hash>`）。
    pub uid: String,
    /// 昵称（用户可改）。
    pub nickname: String,
    /// 用户自己填的备注。
    pub note: String,
    /// 服务商："zai" | "bigmodel" | ""（未指定）。
    pub provider: String,
    /// 用户手动禁用：只不接流量。
    pub disabled: bool,
    pub created_at: String,
    pub last_seen_at: String,
    /// 额度快照；0 = 未知。
    ///
    /// ⚠ ZCode 的额度查询需要 **JWT**（OAuth 登录时才有）。
    /// 只导入了凭证的账号查不到额度 —— 界面应显示"未知"而不是 0，
    /// 因为 0 会被用户误读成"额度耗尽"。
    pub credits: i64,
    pub credits_total: i64,
    /// 最近到期时刻（Unix 秒）；0 = 未知。
    pub expire_at: i64,
}

impl ZcodeAccount {
    /// 转成前端消费的 JSON。
    pub fn to_view(&self) -> Value {
        json!({
            "uid": self.uid,
            "nickname": self.nickname,
            "note": self.note,
            "provider": self.provider,
            "disabled": self.disabled,
            "createdAt": self.created_at,
            "lastSeenAt": self.last_seen_at,
            "credits": self.credits,
            "creditsTotal": self.credits_total,
            "expireAt": self.expire_at,
        })
    }
}

/// 读全部账号（按 uid 排序，保证界面顺序稳定）。
pub fn load_accounts() -> Result<Vec<ZcodeAccount>, String> {
    let path = accounts_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("读取 ZCode 账号库失败: {e}"))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let doc: Value = serde_json::from_str(&raw).map_err(|e| format!("ZCode 账号库格式错误: {e}"))?;
    let arr = doc.get("accounts").and_then(Value::as_array).cloned().unwrap_or_default();

    let mut out: Vec<ZcodeAccount> = arr
        .iter()
        .filter_map(|v| {
            let uid = v.get("uid").and_then(Value::as_str)?.trim().to_string();
            if uid.is_empty() {
                return None;
            }
            Some(ZcodeAccount {
                uid,
                nickname: str_of(v, "nickname"),
                note: str_of(v, "note"),
                provider: str_of(v, "provider"),
                disabled: v.get("disabled").and_then(Value::as_bool).unwrap_or(false),
                created_at: str_of(v, "createdAt"),
                last_seen_at: str_of(v, "lastSeenAt"),
                credits: i64_of(v, "credits"),
                credits_total: i64_of(v, "creditsTotal"),
                expire_at: i64_of(v, "expireAt"),
            })
        })
        .collect();
    out.sort_by(|a, b| a.uid.cmp(&b.uid));
    Ok(out)
}

/// 原子写回账号库（先写临时文件再 rename）。
fn save_accounts(list: &[ZcodeAccount]) -> Result<(), String> {
    ensure_dirs()?;
    let accounts: Vec<Value> = list.iter().map(ZcodeAccount::to_view).collect();
    let doc = json!({ "version": 1, "accounts": accounts });
    let text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;

    let path = accounts_path();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写入 ZCode 账号库失败: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("提交 ZCode 账号库失败: {e}"))
}

/// 新增或更新一个账号（**局部更新**：只覆盖传入的字段）。
pub fn upsert_account(uid: &str, patch: &Value) -> Result<ZcodeAccount, String> {
    let uid = uid.trim();
    if uid.is_empty() {
        return Err("账号 uid 不能为空".into());
    }
    let mut list = load_accounts()?;
    let now = config::utc_iso();

    let acc = match list.iter_mut().find(|a| a.uid == uid) {
        Some(a) => {
            apply_patch(a, patch);
            a.last_seen_at = now.clone();
            a.clone()
        }
        None => {
            let mut a = ZcodeAccount {
                uid: uid.to_string(),
                created_at: now.clone(),
                last_seen_at: now.clone(),
                ..Default::default()
            };
            apply_patch(&mut a, patch);
            a
        }
    };

    if let Some(pos) = list.iter().position(|a| a.uid == uid) {
        list[pos] = acc.clone();
    } else {
        list.push(acc.clone());
    }
    save_accounts(&list)?;
    Ok(acc)
}

/// 把 patch 应用到账号上（只覆盖 patch 里**存在**的键）。
///
/// 局部更新的理由与 qoder_account 一致：界面操作是局部的
///（改备注 / 切换禁用），整体替换会让每次局部操作清空其它字段。
fn apply_patch(a: &mut ZcodeAccount, patch: &Value) {
    let obj = match patch.as_object() {
        Some(o) => o,
        None => return,
    };
    for (k, v) in obj {
        match k.as_str() {
            "nickname" => {
                if let Some(s) = v.as_str() {
                    if !s.trim().is_empty() {
                        a.nickname = s.trim().to_string();
                    }
                }
            }
            // 备注允许清空（用户可能想删掉）
            "note" => {
                if let Some(s) = v.as_str() {
                    a.note = s.trim().to_string();
                }
            }
            "provider" => {
                if let Some(s) = v.as_str() {
                    a.provider = s.trim().to_string();
                }
            }
            "disabled" => {
                if let Some(b) = v.as_bool() {
                    a.disabled = b;
                }
            }
            "credits" => {
                if let Some(n) = v.as_i64() {
                    a.credits = n;
                }
            }
            "creditsTotal" => {
                if let Some(n) = v.as_i64() {
                    a.credits_total = n;
                }
            }
            "expireAt" => {
                if let Some(n) = v.as_i64() {
                    a.expire_at = n;
                }
            }
            _ => {}
        }
    }
}

/// 删除账号（**连同凭证文件**）。
///
/// 只删元信息会留下"孤儿凭证" —— Go 侧的 LoadDir 仍会扫到它，
/// 于是账号在网关里依然可用，用户会疑惑"删了怎么还在跑"。
pub fn delete_account(uid: &str) -> Result<bool, String> {
    let uid = uid.trim();
    let mut list = load_accounts()?;
    let before = list.len();
    list.retain(|a| a.uid != uid);
    let removed = list.len() != before;
    if removed {
        save_accounts(&list)?;
    }
    let cred = auth_dir().join(cred_file_name(uid));
    if cred.exists() {
        std::fs::remove_file(&cred).map_err(|e| format!("删除凭证文件失败: {e}"))?;
        return Ok(true);
    }
    Ok(removed)
}

/// 凭证文件名。
///
/// ⚠ 与 Go 侧的 `Cred.FileBaseName()` 必须一致：uid 本身已带 `zcode-` 前缀，
/// 再拼一次会得到 `zcode-zcode-xxx.json`（Go 侧实测踩过这个坑）。
pub fn cred_file_name(uid: &str) -> String {
    let uid = uid.trim();
    if uid.starts_with("zcode-") {
        format!("{}.json", sanitize_uid(uid))
    } else {
        format!("zcode-{}.json", sanitize_uid(uid))
    }
}

/// 汇总信息（供界面顶部展示）。
pub fn summary() -> Value {
    let accounts = load_accounts().unwrap_or_default();
    let total = accounts.len();
    let disabled = accounts.iter().filter(|a| a.disabled).count();
    let sum_credits: i64 = accounts.iter().map(|a| a.credits).sum();

    json!({
        "total": total,
        "enabled": total - disabled,
        "disabled": disabled,
        "totalCredits": sum_credits,
        "authDir": auth_dir().to_string_lossy(),
        "storeDir": store_dir().to_string_lossy(),
    })
}

/// 磁盘上实际存在的凭证文件（uid 列表）。
pub fn credential_uids() -> Vec<String> {
    let dir = auth_dir();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(rest) = name.strip_prefix("zcode-") {
            if let Some(uid) = rest.strip_suffix(".json") {
                out.push(format!("zcode-{uid}"));
            }
        }
    }
    out.sort();
    out
}

/// 账号视图 + 凭证存在性。
pub fn list_with_credentials() -> Result<Value, String> {
    let accounts = load_accounts()?;
    let on_disk: std::collections::HashSet<String> = credential_uids().into_iter().collect();

    let items: Vec<Value> = accounts
        .iter()
        .map(|a| {
            let mut v = a.to_view();
            v["hasCredential"] = json!(on_disk.contains(&a.uid));
            v
        })
        .collect();

    let known: std::collections::HashSet<&str> = accounts.iter().map(|a| a.uid.as_str()).collect();
    let orphans: Vec<String> = on_disk
        .iter()
        .filter(|u| !known.contains(u.as_str()))
        .cloned()
        .collect();

    Ok(json!({
        "accounts": items,
        "orphanCredentials": orphans,
        "summary": summary(),
    }))
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn i64_of(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// 把 uid 变成安全的文件名片段（防路径穿越）。
pub fn sanitize_uid(uid: &str) -> String {
    let mut out: String = uid
        .trim()
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect();
    while out.contains("..") {
        out = out.replace("..", "__");
    }
    if out.is_empty() {
        out = "unknown".to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_uid_blocks_path_traversal() {
        assert!(!sanitize_uid("../../evil").contains('/'));
        assert!(!sanitize_uid("../../evil").contains(".."));
        assert_eq!(sanitize_uid(""), "unknown");
    }

    #[test]
    fn cred_file_name_matches_go_side() {
        // ⚠ 与 Go 侧 Cred.FileBaseName() 必须一致：uid 已带 zcode- 前缀时
        // 不能再拼一次（Go 侧实测踩过 zcode-zcode-xxx.json 的坑）。
        assert_eq!(cred_file_name("zcode-abc123"), "zcode-abc123.json");
        assert_eq!(cred_file_name("abc123"), "zcode-abc123.json");
    }

    #[test]
    fn apply_patch_only_touches_present_keys() {
        let mut a = ZcodeAccount {
            uid: "u".into(),
            nickname: "旧昵称".into(),
            note: "旧备注".into(),
            credits: 100,
            provider: "zai".into(),
            ..Default::default()
        };
        // 只改备注：其余字段必须保留
        apply_patch(&mut a, &json!({ "note": "新备注" }));
        assert_eq!(a.note, "新备注");
        assert_eq!(a.nickname, "旧昵称", "未出现在 patch 里的字段不该被清空");
        assert_eq!(a.credits, 100, "额度不该因为改备注而丢失");
        assert_eq!(a.provider, "zai", "服务商不该被清空");

        // 备注允许清空
        apply_patch(&mut a, &json!({ "note": "" }));
        assert_eq!(a.note, "");

        // 昵称为空串时不覆盖（上游/用户可能留空）
        apply_patch(&mut a, &json!({ "nickname": "" }));
        assert_eq!(a.nickname, "旧昵称");
    }

    #[test]
    fn apply_patch_ignores_unknown_keys() {
        let mut a = ZcodeAccount::default();
        apply_patch(&mut a, &json!({ "unknownKey": 1, "note": "ok" }));
        assert_eq!(a.note, "ok");
    }

    #[test]
    fn account_view_uses_expected_keys() {
        let a = ZcodeAccount {
            uid: "u1".into(),
            nickname: "n".into(),
            provider: "zai".into(),
            credits: 5,
            credits_total: 10,
            ..Default::default()
        };
        let v = a.to_view();
        // 前端按这些键读；改名会静默显示成 undefined
        for k in ["uid", "nickname", "note", "provider", "disabled", "credits", "creditsTotal"] {
            assert!(v.get(k).is_some(), "视图缺少字段 {k}");
        }
    }
}
