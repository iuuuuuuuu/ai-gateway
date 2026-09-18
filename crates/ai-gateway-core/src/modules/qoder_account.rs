// qoder_account.rs Qoder 账号库（宿主侧）。
//
// 与 doubao_account.rs 同构（同样的「账号元信息 + 凭证分离」设计），
// 但存储位置与 WorkBuddy 的账号库**完全独立** —— Qoder 的凭证形态
// （COSY 签名所需的机器指纹 + dt/drt 令牌）与 WorkBuddy 毫无共同之处，
// 混在一个文件里会让两边的迁移互相干扰。
//
// 目录布局：
//
//	~/.wb-switch/qoder/
//	├── accounts.json          账号元信息（昵称、备注、区域、禁用状态）
//	└── auths/
//	    └── qoder-<uid>.json   COSY 凭证（由 Go 侧 internal/qoder 读写）
//
// 为什么凭证放 auths/ 子目录而不是与 accounts.json 同级：
// Go 侧的 `qoder.LoadDir` 用 glob `qoder*.json` 扫目录，同级会误扫到
// accounts.json（虽然它不匹配前缀，但把凭证与元信息分开更不容易出错）。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::modules::config;

/// Qoder 账号库的根目录。
pub fn store_dir() -> PathBuf {
    config::store_dir().join("qoder")
}

/// 凭证目录（Go 侧的 auth_dir 指向这里）。
pub fn auth_dir() -> PathBuf {
    store_dir().join("auths")
}

/// 账号元信息文件。
fn accounts_path() -> PathBuf {
    store_dir().join("accounts.json")
}

/// 确保目录存在。
pub fn ensure_dirs() -> Result<(), String> {
    std::fs::create_dir_all(auth_dir()).map_err(|e| format!("创建 Qoder 目录失败: {e}"))
}

/// 单个账号的元信息。
///
/// 字段刻意保持精简：**凭证不在这里**（它们在 auths/ 下由 Go 侧管理），
/// 这里只存界面需要展示与筛选的东西。
#[derive(Debug, Clone, Default)]
pub struct QoderAccount {
    /// 上游账号标识（主键）。
    pub uid: String,
    /// 昵称（登录后由上游返回，可能为空）。
    pub nickname: String,
    /// 用户自己填的备注（如「公司号」）。
    pub note: String,
    /// 区域："cn" | "intl" | ""（未指定）。
    pub region: String,
    /// 用户手动禁用：只不接流量，不影响其它操作。
    pub disabled: bool,
    /// 创建时间（ISO 字符串）。
    pub created_at: String,
    /// 最近一次登录/刷新时间。
    pub last_seen_at: String,
    /// 额度快照（剩余 / 总量），由额度查询回填；0 表示未知。
    pub credits: i64,
    pub credits_total: i64,
    /// 最近到期时刻（Unix 秒）；0 = 未知。
    pub expire_at: i64,
}

impl QoderAccount {
    /// 转成前端消费的 JSON（字段名用 snake_case，与其它账号类型一致）。
    pub fn to_view(&self) -> Value {
        json!({
            "uid": self.uid,
            "nickname": self.nickname,
            "note": self.note,
            "region": self.region,
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
pub fn load_accounts() -> Result<Vec<QoderAccount>, String> {
    let path = accounts_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("读取 Qoder 账号库失败: {e}"))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let doc: Value = serde_json::from_str(&raw).map_err(|e| format!("Qoder 账号库格式错误: {e}"))?;
    let arr = doc.get("accounts").and_then(Value::as_array).cloned().unwrap_or_default();

    let mut out: Vec<QoderAccount> = arr
        .iter()
        .filter_map(|v| {
            let uid = v.get("uid").and_then(Value::as_str)?.trim().to_string();
            if uid.is_empty() {
                return None;
            }
            Some(QoderAccount {
                uid,
                nickname: str_of(v, "nickname"),
                note: str_of(v, "note"),
                region: str_of(v, "region"),
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

/// 原子写回账号库。
///
/// 先写临时文件再 rename：直接覆写时若进程被杀，会留下半个 JSON，
/// 下次启动整个账号库都读不出来（丢的是全部账号，不是一条）。
fn save_accounts(list: &[QoderAccount]) -> Result<(), String> {
    ensure_dirs()?;
    let accounts: Vec<Value> = list.iter().map(QoderAccount::to_view).collect();
    let doc = json!({ "version": 1, "accounts": accounts });
    let text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;

    let path = accounts_path();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写入 Qoder 账号库失败: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("提交 Qoder 账号库失败: {e}"))
}

/// 新增或更新一个账号。
///
/// 语义与 doubao_account::upsert_account 一致：
///   · uid 不存在 → 新增（补 created_at）
///   · uid 已存在 → 只更新传入的非空字段（**不清空**用户已有的备注等）
pub fn upsert_account(uid: &str, patch: &Value) -> Result<QoderAccount, String> {
    let uid = uid.trim();
    if uid.is_empty() {
        return Err("账号 uid 不能为空".into());
    }
    let mut list = load_accounts()?;
    let now = config::utc_iso();

    let existing = list.iter_mut().find(|a| a.uid == uid);
    let acc = match existing {
        Some(a) => {
            apply_patch(a, patch);
            a.last_seen_at = now.clone();
            a.clone()
        }
        None => {
            let mut a = QoderAccount {
                uid: uid.to_string(),
                created_at: now.clone(),
                last_seen_at: now.clone(),
                ..Default::default()
            };
            apply_patch(&mut a, patch);
            a
        }
    };

    // 重新组织列表：保留原有顺序，新账号插到末尾
    if let Some(pos) = list.iter().position(|a| a.uid == uid) {
        list[pos] = acc.clone();
    } else {
        list.push(acc.clone());
    }
    save_accounts(&list)?;
    Ok(acc)
}

/// 把 patch 里的字段应用到账号上（只覆盖 patch 里**存在**的键）。
///
/// 为什么不整体替换：界面上的操作是**局部**的（改备注 / 切换禁用 / 刷新额度），
/// 整体替换会让每次局部操作都把其它字段清空 —— 用户改个备注就丢了额度快照。
fn apply_patch(a: &mut QoderAccount, patch: &Value) {
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
            // 备注允许清空（用户可能想删掉备注）——故不判空
            "note" => {
                if let Some(s) = v.as_str() {
                    a.note = s.trim().to_string();
                }
            }
            "region" => {
                if let Some(s) = v.as_str() {
                    a.region = s.trim().to_string();
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

/// 删除账号：**连同凭证文件一起删**。
///
/// 只删元信息会留下一个"孤儿凭证" —— Go 侧的 LoadDir 仍会扫到它，
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
    // 凭证文件：无论元信息是否存在都尝试删（可能只导入了凭证）
    let cred = auth_dir().join(format!("qoder-{}.json", sanitize_uid(uid)));
    if cred.exists() {
        std::fs::remove_file(&cred).map_err(|e| format!("删除凭证文件失败: {e}"))?;
        return Ok(true);
    }
    Ok(removed)
}

/// 汇总信息（供界面顶部展示）。
pub fn summary() -> Value {
    let accounts = load_accounts().unwrap_or_default();
    let total = accounts.len();
    let disabled = accounts.iter().filter(|a| a.disabled).count();
    let with_credits = accounts.iter().filter(|a| a.credits > 0).count();
    let sum_credits: i64 = accounts.iter().map(|a| a.credits).sum();

    json!({
        "total": total,
        "enabled": total - disabled,
        "disabled": disabled,
        "withCredits": with_credits,
        "totalCredits": sum_credits,
        "authDir": auth_dir().to_string_lossy(),
        "storeDir": store_dir().to_string_lossy(),
    })
}

/// 磁盘上实际存在的凭证文件（uid 列表）。
///
/// 用途：界面要区分「有元信息但凭证丢了」与「有凭证但没元信息」——
/// 前者需要重新登录，后者是导入后没登记。两者都显示成"账号"会让用户困惑。
pub fn credential_uids() -> Vec<String> {
    let dir = auth_dir();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(rest) = name.strip_prefix("qoder-") {
            if let Some(uid) = rest.strip_suffix(".json") {
                out.push(uid.to_string());
            }
        }
    }
    out.sort();
    out
}

/// 账号视图 + 凭证存在性（界面需要这个组合）。
pub fn list_with_credentials() -> Result<Value, String> {
    let accounts = load_accounts()?;
    let on_disk: std::collections::HashSet<String> = credential_uids().into_iter().collect();

    let items: Vec<Value> = accounts
        .iter()
        .map(|a| {
            let mut v = a.to_view();
            // hasCredential：凭证文件在不在。为 false 时界面应提示"需重新登录"。
            v["hasCredential"] = json!(on_disk.contains(&a.uid));
            v
        })
        .collect();

    // 有凭证但没元信息的：列出来（导入后未登记的情况）
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

/// 把 uid 变成安全的文件名片段（与 Go 侧 sanitizeUID 口径一致）。
///
/// 两端口径必须一致，否则宿主写的文件名 Go 侧扫不到（或反之）。
pub fn sanitize_uid(uid: &str) -> String {
    let mut out: String = uid
        .trim()
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect();
    // ".." 单独处理（路径穿越）
    while out.contains("..") {
        out = out.replace("..", "__");
    }
    if out.is_empty() {
        out = "unknown".to_string();
    }
    out
}

/// 按区域分组统计（供界面筛选）。
pub fn count_by_region() -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for a in load_accounts().unwrap_or_default() {
        let key = if a.region.is_empty() { "unknown".to_string() } else { a.region.clone() };
        *m.entry(key).or_insert(0) += 1;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_uid_blocks_path_traversal() {
        // 安全项：uid 来自上游，直接拼进文件名会写到目录外
        assert!(!sanitize_uid("../../evil").contains('/'));
        assert!(!sanitize_uid("../../evil").contains('\\'));
        assert!(!sanitize_uid("../../evil").contains(".."));
        assert_eq!(sanitize_uid(""), "unknown");
        assert_eq!(sanitize_uid("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_uid("normal-uid"), "normal-uid");
    }

    #[test]
    fn sanitize_uid_matches_go_side() {
        // 与 Go 侧 sanitizeUID 的口径必须一致，否则宿主写的文件名 Go 侧扫不到。
        // Go 侧的规则：/ \ : * ? " < > | 与 ".." 都换成下划线。
        assert_eq!(sanitize_uid("with:colon"), "with_colon");
        assert_eq!(sanitize_uid("q?mark"), "q_mark");
    }

    #[test]
    fn apply_patch_only_touches_present_keys() {
        let mut a = QoderAccount {
            uid: "u".into(),
            nickname: "旧昵称".into(),
            note: "旧备注".into(),
            credits: 100,
            ..Default::default()
        };
        // 只改备注：昵称与额度都必须保留
        apply_patch(&mut a, &json!({ "note": "新备注" }));
        assert_eq!(a.note, "新备注");
        assert_eq!(a.nickname, "旧昵称", "未出现在 patch 里的字段不该被清空");
        assert_eq!(a.credits, 100, "额度不该因为改备注而丢失");

        // 备注允许清空（用户可能想删掉备注）
        apply_patch(&mut a, &json!({ "note": "" }));
        assert_eq!(a.note, "");

        // 昵称为空串时**不覆盖**（上游有时返回空昵称，不该把已有的抹掉）
        apply_patch(&mut a, &json!({ "nickname": "" }));
        assert_eq!(a.nickname, "旧昵称");
    }

    #[test]
    fn apply_patch_ignores_unknown_keys() {
        let mut a = QoderAccount::default();
        // 未知键不得 panic、也不得写入
        apply_patch(&mut a, &json!({ "unknownKey": 1, "note": "ok" }));
        assert_eq!(a.note, "ok");
    }

    #[test]
    fn account_view_uses_snake_case_keys() {
        let a = QoderAccount {
            uid: "u1".into(),
            nickname: "n".into(),
            credits: 5,
            credits_total: 10,
            ..Default::default()
        };
        let v = a.to_view();
        // 前端按这些键读；改名会静默显示成 undefined
        for k in ["uid", "nickname", "note", "region", "disabled", "credits", "creditsTotal"] {
            assert!(v.get(k).is_some(), "视图缺少字段 {k}");
        }
    }
}
