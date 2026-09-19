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
    /// 头像 URL（来自客户端登录态）；空 = 没有。
    ///
    /// 使用者的反馈：「已授权后,也不显示头像」—— 头像就在客户端
    /// `credentials.json` 的 `oauth:*:user_info` 里，登录后刷新即可补上。
    pub avatar_url: String,
    /// 上游**账号**标识（如 `19331730795565300`）；空 = 未知。
    ///
    /// 用途：识别「同一账号的多把 API key」—— 它们的 `uid` 不同
    ///（uid 是凭证哈希），但 `account_id` 相同，在用户看来就是重复。
    /// 只**标注**，不自动删（多把 key 可能是故意的）。
    pub account_id: String,
    /// 该账号**实际可用**的模型 id 列表（刷新账号时查得）。
    ///
    /// 用途：
    ///   1. 界面上显示"这个账号能用哪些模型"
    ///   2. 汇总进网关的 `pool.product_models` → `/v1/models` 的
    ///      `channels` 字段（"这个模型来自哪个平台"）
    ///
    /// 空 = 没查过（与"查了但一个都没有"是两回事，但这里都表现为空 ——
    /// 界面上按"未刷新"提示，见 refresh_account 的返回值）。
    pub models: Vec<String>,
    /// **套餐整体**到期（Unix 秒）；0 = 无套餐或未知。
    ///
    /// ⚠ 与 `expire_at` 不是一回事，**不能混用**：
    ///
    /// ```text
    /// expire_at        各模型桶的每日周期结束（实测当天 23:59:59）
    /// plan_expire_at   套餐整体到期（实测 2026-09-23 23:59:59）
    /// ```
    ///
    /// 所有者的实测反馈：「这个体验套餐是 9月23号23:59 过期时间，
    /// 这是我刚登录的新账号赠送的额度，要区分好」——
    /// 那个时刻一直藏在 `billing/balance` 的 `plans[].ends_at` 里，
    /// 而我们此前**完全没读 plans**，所以界面上只显示了每天的重置时间。
    pub plan_expire_at: i64,
    /// 套餐类型：`trial`（体验套餐）/ `paid`（付费）/ `api_key` / `unknown`。
    ///
    /// 判据由 Go 侧 `plan.go` 按上游静态配置（`startPlanPreview` 的赠送量）
    /// 比对得出，**不硬编码数字**。取不到配置时为 `unknown`（不猜）。
    ///
    /// 为什么要区分：体验套餐**每日重置且到期后作废**，付费套餐随订阅续期。
    /// 同样显示"还有 800 万"，两者的含义完全不同。
    pub plan_kind: String,
    /// 生效中的套餐明细（上游原样透传，供界面展示套餐名与说明）。
    pub plans: Vec<Value>,
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
            "planExpireAt": self.plan_expire_at,
            "planKind": self.plan_kind,
            "plans": self.plans,
            "avatarUrl": self.avatar_url,
            "accountId": self.account_id,
            "models": self.models,
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
                // 套餐整体到期 —— 与 expire_at 分开存（语义不同）
                plan_expire_at: i64_of(v, "planExpireAt"),
                plan_kind: str_of(v, "planKind"),
                plans: v
                    .get("plans")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                avatar_url: str_of(v, "avatarUrl"),
                account_id: str_of(v, "accountId"),
                models: v
                    .get("models")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default(),
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

    // 「停止接流量」必须**同时写进凭证文件** —— 网关只扫 auths/，
    // 不读本文件。漏了这一步，界面上的开关就是个摆设（实测确认过）。
    if patch.get("disabled").and_then(Value::as_bool).is_some() {
        // 同步失败**不阻断**元信息保存：账号库已经存好了，
        // 而凭证文件可能因为用户手工删过而不存在（那不是错误）。
        // 但要**如实告知** —— 静默失败会让开关看起来生效了而实际没有。
        if let Err(e) = sync_no_route_to_credential(uid, acc.disabled) {
            return Err(format!(
                "账号信息已保存，但「停止接流量」标记未能写入凭证文件（{e}）。\
                 网关可能仍会把请求路由到这个账号。"
            ));
        }
    }

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
            // 套餐整体到期 —— 与 expireAt 语义不同，**不要**互相覆盖。
            "planExpireAt" => {
                if let Some(n) = v.as_i64() {
                    a.plan_expire_at = n;
                }
            }
            // 套餐类型。空串也接受（那是"这次判不出来"的意思，
            // 必须能覆盖上一次的判断结果）。
            "planKind" => {
                if let Some(s) = v.as_str() {
                    a.plan_kind = s.trim().to_string();
                }
            }
            "plans" => {
                if let Some(arr) = v.as_array() {
                    a.plans = arr.clone();
                }
            }
            // 身份字段：由 `refresh_account` 从客户端登录态补上。
            // 空串也接受（那是"清除头像"的意思，不要静默忽略）。
            "avatarUrl" => {
                if let Some(s) = v.as_str() {
                    a.avatar_url = s.to_string();
                }
            }
            "accountId" => {
                if let Some(s) = v.as_str() {
                    a.account_id = s.to_string();
                }
            }
            // 模型清单：由 `refresh_account` 写入。空数组也接受
            //（那是"查到了但一个模型都没有"的意思，必须能覆盖旧值）。
            "models" => {
                if let Some(arr) = v.as_array() {
                    a.models = arr
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
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

/// 更新账号时，把「用户手动禁用」**写进凭证文件**（`account.no_route`）。
///
/// ## 为什么必须下发到凭证文件
///
/// 网关的账号池是**扫描凭证目录**（`auths/`）建立的，而宿主的
/// `accounts.json` 网关**根本不读**。于是界面上的「停止接流量」开关
/// 只改了宿主自己的库 —— 网关完全不知道，那个账号照常接流量。
///
/// 这与 WorkBuddy 用的是同一个机制（见 `gateway.rs` 的
/// `account.no_route` 注释），复用它可以避免第二套语义。
///
/// 标记名用 `no_route` 而不是 `disabled`：网关自己也有一组 disabled
///（session 死 / 额度冻结），两者语义不同，混用会出问题。
///
/// ## 幂等
///
/// 启用时**删掉**这个键（而不是写 `false`）—— 与 WorkBuddy 侧一致，
/// 也让"启用账号的凭证文件里没有这个键"成为可断言的事实。
pub fn sync_no_route_to_credential(uid: &str, disabled: bool) -> Result<bool, String> {
    let path = auth_dir().join(cred_file_name(uid));
    if !path.exists() {
        // 凭证不存在（用户只加了元信息）—— 不是错误，没什么可同步的
        return Ok(false);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("读取凭证失败: {e}"))?;
    let mut doc: Value = serde_json::from_str(&raw).map_err(|e| format!("凭证格式错误: {e}"))?;

    let account = doc
        .as_object_mut()
        .ok_or_else(|| "凭证顶层不是对象".to_string())?
        .entry("account")
        .or_insert_with(|| json!({}));
    let account = account
        .as_object_mut()
        .ok_or_else(|| "凭证的 account 不是对象".to_string())?;

    if disabled {
        account.insert("no_route".to_string(), json!(true));
    } else {
        account.remove("no_route");
    }
    // uid 一并写入：Go 侧优先读 uid 字段，缺了会回退到文件名派生
    account
        .entry("uid".to_string())
        .or_insert_with(|| json!(uid.trim()));

    let text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写入凭证失败: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("提交凭证失败: {e}"))?;
    Ok(true)
}

/// 从凭证文件里读 `no_route` 标记（供界面显示上与网关对齐）。
pub fn credential_no_route(uid: &str) -> bool {
    let path = auth_dir().join(cred_file_name(uid));
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(doc) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    doc.get("account")
        .and_then(|a| a.get("no_route"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
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

    /// 套餐三件套必须能被 patch 写入。
    ///
    /// # 为什么要专门守它（所有者反馈：「智谱的到期时间还没显示出来」）
    ///
    /// 我排查时一度以为 `apply_patch` 漏了这三个键，**加了重复分支** ——
    /// 编译器报 `unreachable_patterns` 才发现它本来就支持（见本函数上方
    /// 的 `planExpireAt` / `planKind` / `plans` 三个分支）。
    ///
    /// 真正的缺陷只在**写入端**（`zcode_login::refresh_account` 的 patch
    /// 没带这三个键）。这条测试的作用是把"读取端确实支持"这件事**钉住** ——
    /// 若哪天有人误删了这三个分支，这条会红，而不是让界面静默失去到期时间。
    #[test]
    fn apply_patch_writes_plan_fields() {
        let mut a = ZcodeAccount::default();
        apply_patch(
            &mut a,
            &json!({
                "planExpireAt": 1789866000i64,
                "planKind": "paid",
                "plans": [{ "name": "ZCode Weekend Build", "planId": "zcode-v3-start-plan-wk-0918" }],
            }),
        );
        assert_eq!(a.plan_expire_at, 1789866000, "套餐到期必须能被写入（否则界面无到期时间）");
        assert_eq!(a.plan_kind, "paid", "套餐类型必须能被写入（界面要区分个人/体验套餐）");
        assert_eq!(a.plans.len(), 1, "套餐明细必须能被写入");
    }

    /// 套餐字段允许被空值覆盖 —— 这是**有意**的语义。
    ///
    /// # 为什么和 `models` 的取舍不同
    ///
    /// `models` 是「空数组不覆盖」（避免一次网络抖动抹掉好数据），
    /// 而这三个字段**允许**空值覆盖，理由是它们表达的是**当下判断**：
    ///
    ///	· 套餐**已经到期**了 → 就该显示"已过期"，而不是留着上次的到期日
    ///	· 这次判不出类型 → 空串比留着旧判断更诚实
    ///
    /// 代码里的注释写明了这个意图（"空串也接受……必须能覆盖上一次的
    /// 判断结果"）。故这里**反向**钉住：不是"不该覆盖"，而是"必须能覆盖"。
    /// 我第一版把这条写反了（断言不该覆盖），测试红了 —— 那是我的假设错，
    /// 不是代码错。
    ///
    /// 真正要防的"抹掉好数据"由**写入端**负责：`refresh_account` 只在
    /// 查到值时才把键放进 patch（见 zcode_login.rs 的 `enrich` 段注释）。
    #[test]
    fn apply_patch_allows_clearing_plan_fields() {
        let mut a = ZcodeAccount {
            plan_expire_at: 1789866000,
            plan_kind: "paid".into(),
            plans: vec![json!({ "name": "ZCode Weekend Build" })],
            ..Default::default()
        };
        apply_patch(&mut a, &json!({ "planExpireAt": 0i64, "planKind": "", "plans": [] }));
        assert_eq!(a.plan_expire_at, 0, "套餐到期应能被清空（已到期的场景）");
        assert_eq!(a.plan_kind, "", "套餐类型应能被清空（判不出来的场景）");
        assert!(a.plans.is_empty(), "套餐明细应能被清空");
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

    // 「停止接流量」开关必须**写进凭证文件**（`account.no_route`）。
    //
    // ## 为什么必须锁住这一条
    //
    // 网关的账号池是**扫描凭证目录**（auths/）建立的，宿主的 accounts.json
    // 网关**根本不读**。故只改账号库的话，界面上的开关是个**摆设** ——
    // 用户以为停掉了，网关照常把请求路由过去。
    //
    // 这与 WorkBuddy 用同一个机制（`account.no_route`），复用可避免第二套语义。
    #[test]
    fn disabled_flag_is_written_to_credential_file() {
        let _iso = crate::modules::config::test_isolation::Isolated::new("zcode-noroute");
        ensure_dirs().unwrap();

        let uid = "zcode-noroute-test01";
        let cred_path = auth_dir().join(cred_file_name(uid));
        std::fs::write(
            &cred_path,
            serde_json::to_string_pretty(&json!({
                "uid": uid, "provider": "zai", "credential": "k.s"
            }))
            .unwrap(),
        )
        .unwrap();

        // ① 禁用 → 凭证文件里出现 no_route
        sync_no_route_to_credential(uid, true).unwrap();
        assert!(
            credential_no_route(uid),
            "禁用后凭证文件里必须有 account.no_route —— \
             否则网关看不到这个标记，账号照常接流量"
        );

        // ② 启用 → 标记被**删除**（而不是写成 false）
        sync_no_route_to_credential(uid, false).unwrap();
        assert!(
            !credential_no_route(uid),
            "取消禁用后标记必须清除，否则该账号永远不会被选号"
        );
        let raw = std::fs::read_to_string(&cred_path).unwrap();
        let doc: Value = serde_json::from_str(&raw).unwrap();
        assert!(
            doc.get("account")
                .and_then(|a| a.get("no_route"))
                .is_none(),
            "启用时应**删除**该键，而不是写 false（保持与 WorkBuddy 一致，\
             也让「启用账号没有这个键」成为可断言的事实）"
        );

        // ③ 凭证不存在时**不报错**（用户只加了元信息是正常情况）
        assert!(
            sync_no_route_to_credential("zcode-nonexistent", true).is_ok(),
            "凭证文件不存在时不该报错 —— 那是「只加了元信息」的正常状态"
        );

        let _ = std::fs::remove_file(&cred_path);
    }
}
