//! 豆包账号库与凭证管理。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/commands/doubao.rs`（账号 CRUD + 抓包回写）。
//!
//! ## 凭证就是密码
//!
//! 豆包的 `sessionid` 等价于账号密码（拿到即可完全接管会话），因此：
//! - 界面展示一律脱敏（[`account_view`] 只给掩码）
//! - 明文只在「编辑凭证」与「后端发请求」两条路径上出现
//! - 日志里只记录长度，绝不记录值
//!
//! ## 抓包凭证回写为什么必须按抓包文件自己的 uid 定位
//!
//! 早期实现用「uid 探测链」定位目标账号，而凭证来自抓包文件 —— 两者来源不同，
//! 实测导致账号 908 与 232 拿到了**同一个** sessionid（跨账号污染）。
//! 现在只信抓包文件里 `multi_sids` 解析出的 uid。

use serde_json::{json, Value};

use crate::modules::config;

/// 账号库文件（`<store_dir>/doubao_accounts.json`）。
pub fn accounts_file() -> std::path::PathBuf {
    config::store_dir().join("doubao_accounts.json")
}

/// 抓包凭证文件（`<store_dir>/doubao_captured_credential.json`）。
pub fn captured_file() -> std::path::PathBuf {
    config::store_dir().join("doubao_captured_credential.json")
}

/// 读取账号池。
pub fn load_pool() -> Value {
    std::fs::read_to_string(accounts_file())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({ "accounts": [], "last_keepalive_at": null }))
}

/// 写回账号池（原子写）。
pub fn save_pool(pool: &Value) -> Result<(), String> {
    std::fs::create_dir_all(config::store_dir()).map_err(|e| e.to_string())?;
    let content = serde_json::to_string_pretty(pool).map_err(|e| e.to_string())?;
    config::atomic_write(&accounts_file(), &content).map_err(|e| e.to_string())
}

/// 账号列表（明文形态，仅供后端使用）。
pub fn load_accounts() -> Vec<Value> {
    load_pool()
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// 按 user_id 查找账号。
pub fn find_account(user_id: &str) -> Option<Value> {
    let user_id = user_id.trim();
    load_accounts()
        .into_iter()
        .find(|a| a.get("user_id").and_then(Value::as_str) == Some(user_id))
}

/// uid 是否可安全用作路径片段。
///
/// **每个** uid → 路径的入口都必须先过这一关：uid 来自抓包/本地存储，
/// 属于外部输入，含 `..` 或分隔符时可以越出数据目录。
pub fn ensure_uid_safe(uid: &str) -> Result<(), String> {
    let uid = uid.trim();
    if uid.is_empty() {
        return Err("缺少账号标识（userId）".to_string());
    }
    if uid.len() > 64 {
        return Err("账号标识过长（超过 64 字符）".to_string());
    }
    if uid.contains("..") {
        return Err("账号标识含非法片段 `..`".to_string());
    }
    if !uid
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err("账号标识只能包含字母、数字、下划线与连字符".to_string());
    }
    Ok(())
}

/// 脱敏：保留末 `keep` 位。
pub fn mask_secret(value: &str, keep: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() <= keep {
        return "*".repeat(chars.len());
    }
    format!(
        "{}{}",
        "*".repeat(chars.len() - keep),
        chars[chars.len() - keep..].iter().collect::<String>()
    )
}

/// 会话状态。
pub fn session_state(acc: &Value) -> &'static str {
    let has_session = acc
        .get("session_id")
        .and_then(Value::as_str)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !has_session {
        return "none";
    }
    match acc.get("expired").and_then(Value::as_bool) {
        Some(true) => "expired",
        Some(false) => "ok",
        None => "unknown",
    }
}

/// 账号的展示视图（**凭证脱敏**）。
pub fn account_view(acc: &Value) -> Value {
    let session_id = acc.get("session_id").and_then(Value::as_str).unwrap_or("");
    let sid_guard = acc.get("sid_guard").and_then(Value::as_str).unwrap_or("");
    let ttwid = acc.get("ttwid").and_then(Value::as_str).unwrap_or("");
    json!({
        "userId": acc.get("user_id"),
        "name": acc.get("name"),
        "note": acc.get("note"),
        "addedAt": acc.get("added_at"),
        "lastActiveAt": acc.get("last_active_at"),
        // 凭证掩码：只给末 4 位供用户核对，不给全文
        "sessionIdMasked": mask_secret(session_id, 4),
        "sidGuardMasked": mask_secret(sid_guard, 4),
        "ttwidMasked": mask_secret(ttwid, 4),
        "hasSessionId": !session_id.is_empty(),
        "hasTtwid": !ttwid.is_empty(),
        "sessionExpireAt": acc.get("session_expire_at"),
        "expired": acc.get("expired"),
        "sessionState": session_state(acc),
        "sessionSource": acc.get("session_source"),
        "cookiesSyncedAt": acc.get("cookies_synced_at"),
        "lastRenewAt": acc.get("last_renew_at"),
        // 额度缓存
        "quotaLevel": acc.get("quota_level"),
        "quotaExpireAt": acc.get("quota_expire_at"),
        "quotaSummary": acc.get("quota_summary"),
        "quotaCheckedAt": acc.get("quota_checked_at"),
    })
}

/// 新增或更新账号。
///
/// `auto_create=false` 时，uid 不在池中则**不创建**（抓包回写用）——
/// 浏览器网页版/其他字节系应用也会产生豆包 cookie，无差别建号会污染账号池。
pub fn upsert_account(
    user_id: &str,
    name: Option<&str>,
    note: Option<&str>,
    auto_create: bool,
) -> Result<Value, String> {
    ensure_uid_safe(user_id)?;
    let user_id = user_id.trim();
    let mut pool = load_pool();
    let accounts = pool
        .get_mut("accounts")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "账号池结构异常".to_string())?;

    if let Some(existing) = accounts
        .iter_mut()
        .find(|a| a.get("user_id").and_then(Value::as_str) == Some(user_id))
    {
        let obj = existing.as_object_mut().ok_or("账号记录格式异常")?;
        if let Some(n) = name.map(str::trim).filter(|s| !s.is_empty()) {
            obj.insert("name".to_string(), json!(n));
        }
        if let Some(n) = note {
            obj.insert("note".to_string(), json!(n.trim()));
        }
        let updated = existing.clone();
        save_pool(&pool)?;
        return Ok(updated);
    }

    if !auto_create {
        return Err(format!("账号 {user_id} 不在账号池中，已跳过（不自动创建）"));
    }

    let now = config::utc_iso();
    let acc = json!({
        "user_id": user_id,
        "name": name.map(str::trim).filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("豆包_{}", &user_id.chars().take(8).collect::<String>())),
        "note": note.map(|s| s.trim().to_string()).unwrap_or_default(),
        "added_at": now,
    });
    accounts.push(acc.clone());
    save_pool(&pool)?;
    Ok(acc)
}

/// 删除账号。
pub fn delete_account(user_id: &str) -> Result<(), String> {
    let mut pool = load_pool();
    let accounts = pool
        .get_mut("accounts")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "账号池结构异常".to_string())?;
    let before = accounts.len();
    accounts.retain(|a| a.get("user_id").and_then(Value::as_str) != Some(user_id.trim()));
    if accounts.len() == before {
        return Err("账号不存在".to_string());
    }
    save_pool(&pool)
}

/// 就地修改某账号的字段（闭包拿到可变的账号对象）。
pub fn mutate_account<F>(user_id: &str, f: F) -> Result<Value, String>
where
    F: FnOnce(&mut serde_json::Map<String, Value>),
{
    let mut pool = load_pool();
    let accounts = pool
        .get_mut("accounts")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "账号池结构异常".to_string())?;
    let acc = accounts
        .iter_mut()
        .find(|a| a.get("user_id").and_then(Value::as_str) == Some(user_id.trim()))
        .ok_or_else(|| format!("账号 {user_id} 不存在"))?;
    let obj = acc.as_object_mut().ok_or("账号记录格式异常")?;
    f(obj);
    let updated = acc.clone();
    save_pool(&pool)?;
    Ok(updated)
}

/// 更新保活时间戳（池级）。
pub fn set_last_keepalive(at: &str) -> Result<(), String> {
    let mut pool = load_pool();
    if let Some(obj) = pool.as_object_mut() {
        obj.insert("last_keepalive_at".to_string(), json!(at));
    }
    save_pool(&pool)
}

/// 池级保活时间戳。
pub fn last_keepalive_at() -> Option<String> {
    load_pool()
        .get("last_keepalive_at")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 应用凭证到账号。
///
/// `source` 记录凭证来源（`live` / `snapshot` / `manual` / `proxy`），
/// 便于用户排查「这个 sessionid 是哪来的」。
pub fn apply_credential(
    user_id: &str,
    session_id: Option<&str>,
    sid_guard: Option<&str>,
    ttwid: Option<&str>,
    source: &str,
    auto_create: bool,
) -> Result<Value, String> {
    ensure_uid_safe(user_id)?;
    let user_id = user_id.trim();
    // uid 不在池中且不允许创建 → 明确拒绝（抓包回写路径）
    if !auto_create && find_account(user_id).is_none() {
        return Err(format!("账号 {user_id} 不在账号池中，已跳过"));
    }
    if auto_create && find_account(user_id).is_none() {
        upsert_account(user_id, None, None, true)?;
    }

    let now = config::utc_iso();
    mutate_account(user_id, |obj| {
        match session_id.map(str::trim) {
            // 显式传空串 = 清除凭证
            Some("") => {
                obj.insert("session_id".to_string(), Value::Null);
            }
            Some(sid) => {
                obj.insert("session_id".to_string(), json!(sid));
            }
            None => {}
        }
        if let Some(sg) = sid_guard {
            let sg = sg.trim();
            if sg.is_empty() {
                obj.insert("sid_guard".to_string(), Value::Null);
                obj.insert("session_expire_at".to_string(), Value::Null);
            } else {
                // 只有 sid_guard **确实变了**才作废旧的过期判定。
                //
                // 早期实现无条件清空，导致「改个备注点保存」也会把 `session_expire_at`
                // 抹掉 —— 界面上的「会话到期」整行随之消失，而且这条路径上没有任何
                // 地方会把它重新算回来（`parse_sid_guard` 只在续期拿到新 guard 时才写），
                // 于是过期时间会一直空到下次服务端下发新 guard 为止。
                let changed = obj.get("sid_guard").and_then(Value::as_str) != Some(sg);
                obj.insert("sid_guard".to_string(), json!(sg));
                if changed {
                    // 服务端给了新 guard 但没给新 sessionid 时，过期时间可由 guard 自算
                    match crate::modules::doubao_session::parse_sid_guard(sg) {
                        Some(expire) => {
                            obj.insert("session_expire_at".to_string(), json!(expire));
                        }
                        None => {
                            obj.insert("session_expire_at".to_string(), Value::Null);
                        }
                    }
                }
            }
        }
        // ttwid 只在显式给值时才覆盖（抓包可能没带 ttwid，清掉会让对话导出失效）
        if let Some(tw) = ttwid.map(str::trim).filter(|s| !s.is_empty()) {
            obj.insert("ttwid".to_string(), json!(tw));
        }
        obj.insert("expired".to_string(), Value::Null);
        obj.insert("session_source".to_string(), json!(source));
        obj.insert("cookies_synced_at".to_string(), json!(now));
    })
}

// ---------------------------------------------------------------------------
// 抓包凭证（MITM 代理写入，凭证回写读取）
// ---------------------------------------------------------------------------

/// 读取最近一次抓包凭证。
pub fn load_captured() -> Option<Value> {
    let raw = std::fs::read_to_string(captured_file()).ok()?;
    serde_json::from_str::<Value>(raw.trim_start_matches('\u{feff}')).ok()
}

/// 写入抓包凭证。
pub fn save_captured(captured: &Value) -> Result<(), String> {
    std::fs::create_dir_all(config::store_dir()).map_err(|e| e.to_string())?;
    let content = serde_json::to_string_pretty(captured).map_err(|e| e.to_string())?;
    config::atomic_write(&captured_file(), &content).map_err(|e| e.to_string())
}

/// 把抓包凭证回写到账号池。
///
/// 三条硬性约束（都来自实测踩坑）：
/// 1. **目标账号取抓包文件自己的 uid** —— 用探测链定位会让两个账号拿到同一凭证
/// 2. **绝不自动建号** —— 网页版/其他字节应用的 cookie 会污染账号池
/// 3. **幂等** —— `session_id`、`sid_guard`、`ttwid` 三者都没变时不写盘，
///    避免每 20 秒的轮询把文件时间戳刷得毫无意义
pub fn auto_apply_captured() -> Result<Option<Value>, String> {
    let Some(captured) = load_captured() else {
        return Ok(None);
    };
    let session_id = captured
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if session_id.is_empty() {
        return Ok(None);
    }
    let uid = captured
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if uid.is_empty() {
        // 没有 multi_sids 就无法可靠归属，宁可不写
        return Ok(None);
    }
    if ensure_uid_safe(uid).is_err() {
        return Ok(None);
    }
    let Some(existing) = find_account(uid) else {
        // 不自动建号
        return Ok(None);
    };

    let sid_guard = captured.get("sid_guard").and_then(Value::as_str);
    let ttwid = captured.get("ttwid").and_then(Value::as_str);
    let same = |key: &str, incoming: Option<&str>| -> bool {
        let current = existing.get(key).and_then(Value::as_str).unwrap_or("");
        match incoming.map(str::trim) {
            // 抓包没带这个字段 → 不算变化
            None | Some("") => true,
            Some(v) => v == current,
        }
    };
    if same("session_id", Some(session_id))
        && same("sid_guard", sid_guard)
        && same("ttwid", ttwid)
    {
        return Ok(None);
    }

    let updated = apply_credential(uid, Some(session_id), sid_guard, ttwid, "proxy", false)?;
    Ok(Some(account_view(&updated)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把数据目录隔离到临时目录（否则会污染真实账号池，且并发测试会互相踩）。
    use crate::modules::config::test_isolation::Isolated;

    #[test]
    fn uid_安全校验拦截路径穿越() {
        assert!(ensure_uid_safe("1234567890").is_ok());
        assert!(ensure_uid_safe("user-name_1").is_ok());
        assert!(ensure_uid_safe("").is_err());
        assert!(ensure_uid_safe("  ").is_err());
        assert!(ensure_uid_safe("..").is_err());
        assert!(ensure_uid_safe("../../etc/passwd").is_err());
        assert!(ensure_uid_safe("a/../b").is_err());
        assert!(ensure_uid_safe("a\\b").is_err());
        assert!(ensure_uid_safe("a/b").is_err());
        assert!(ensure_uid_safe("a b").is_err());
        assert!(ensure_uid_safe(&"x".repeat(65)).is_err());
        assert!(ensure_uid_safe(&"x".repeat(64)).is_ok());
    }

    #[test]
    fn 脱敏只保留末几位() {
        assert_eq!(mask_secret("abcdefgh", 4), "****efgh");
        assert_eq!(mask_secret("abcd", 4), "****");
        assert_eq!(mask_secret("ab", 4), "**");
        assert_eq!(mask_secret("", 4), "");
    }

    #[test]
    fn 账号视图不含凭证明文() {
        let acc = json!({
            "user_id": "123",
            "name": "测试",
            "session_id": "super-secret-session-value",
            "sid_guard": "guard-secret",
            "ttwid": "ttwid-secret",
        });
        let view = account_view(&acc);
        let text = serde_json::to_string(&view).unwrap();
        assert!(!text.contains("super-secret-session-value"), "sessionid 明文泄露");
        assert!(!text.contains("guard-secret"), "sid_guard 明文泄露");
        assert!(!text.contains("ttwid-secret"), "ttwid 明文泄露");
        assert_eq!(view["hasSessionId"], true);
        assert_eq!(view["sessionState"], "unknown");
    }

    #[test]
    fn 会话状态判定() {
        assert_eq!(session_state(&json!({})), "none");
        assert_eq!(session_state(&json!({"session_id": ""})), "none");
        assert_eq!(session_state(&json!({"session_id": "x"})), "unknown");
        assert_eq!(session_state(&json!({"session_id": "x", "expired": false})), "ok");
        assert_eq!(session_state(&json!({"session_id": "x", "expired": true})), "expired");
    }

    #[test]
    fn 账号增删改查() {
        let _iso = Isolated::new("crud");
        assert!(load_accounts().is_empty());

        let acc = upsert_account("123456", Some("我的豆包"), Some("备注"), true).unwrap();
        assert_eq!(acc["name"], "我的豆包");
        assert_eq!(acc["note"], "备注");
        assert_eq!(load_accounts().len(), 1);

        // 更新不重复插入
        upsert_account("123456", Some("改名了"), None, true).unwrap();
        assert_eq!(load_accounts().len(), 1);
        assert_eq!(find_account("123456").unwrap()["name"], "改名了");

        // 不自动创建时拒绝
        assert!(upsert_account("999", None, None, false).is_err());
        assert!(find_account("999").is_none());

        delete_account("123456").unwrap();
        assert!(load_accounts().is_empty());
        assert!(delete_account("123456").is_err(), "删除不存在的账号应报错");
    }

    #[test]
    fn 非法_uid_无法入库() {
        let _iso = Isolated::new("baduid");
        assert!(upsert_account("../evil", None, None, true).is_err());
        assert!(upsert_account("", None, None, true).is_err());
        assert!(load_accounts().is_empty(), "非法 uid 不得写入任何记录");
    }

    #[test]
    fn 凭证应用与清除() {
        let _iso = Isolated::new("cred");
        upsert_account("123456", None, None, true).unwrap();

        apply_credential("123456", Some("sid-1"), Some("guard-1"), Some("tw-1"), "manual", true)
            .unwrap();
        let acc = find_account("123456").unwrap();
        assert_eq!(acc["session_id"], "sid-1");
        assert_eq!(acc["sid_guard"], "guard-1");
        assert_eq!(acc["ttwid"], "tw-1");
        assert_eq!(acc["session_source"], "manual");

        // 传空串 = 清除 sessionid
        apply_credential("123456", Some(""), None, None, "manual", true).unwrap();
        let acc = find_account("123456").unwrap();
        assert!(acc["session_id"].is_null());

        // ttwid 为 None 时不覆盖已有值
        apply_credential("123456", Some("sid-2"), None, None, "manual", true).unwrap();
        let acc = find_account("123456").unwrap();
        assert_eq!(acc["ttwid"], "tw-1", "未提供 ttwid 时不得清掉原值");
    }

    #[test]
    fn 重复提交同一_sid_guard_不清掉会话到期时间() {
        // 回归：早期实现只要 sid_guard 非空就无条件把 `session_expire_at` 置空。
        // 于是「改个备注点保存」也会让界面上的「会话到期」整行消失，
        // 而且没有任何路径会把它算回来（只有续期拿到新 guard 时才写）。
        let _iso = Isolated::new("guard-expire");
        upsert_account("123456", None, None, true).unwrap();

        let guard = "abc|1767225600|2592000";
        apply_credential("123456", Some("sid-1"), Some(guard), None, "manual", true).unwrap();
        let expire = find_account("123456").unwrap()["session_expire_at"]
            .as_str()
            .map(str::to_string);
        assert!(
            expire.is_some(),
            "首次写入 sid_guard 时应由 guard 算出会话到期时间"
        );

        // 再次提交**同一个** guard（模拟只改备注后保存）→ 到期时间必须保留
        apply_credential("123456", Some("sid-1"), Some(guard), None, "manual", true).unwrap();
        assert_eq!(
            find_account("123456").unwrap()["session_expire_at"]
                .as_str()
                .map(str::to_string),
            expire,
            "sid_guard 未变化时不得清掉会话到期时间"
        );

        // guard 真的换了 → 到期时间随新 guard 重算
        let new_guard = "def|1767225600|86400";
        apply_credential("123456", Some("sid-1"), Some(new_guard), None, "manual", true).unwrap();
        let new_expire = find_account("123456").unwrap()["session_expire_at"]
            .as_str()
            .map(str::to_string);
        assert!(new_expire.is_some());
        assert_ne!(new_expire, expire, "换 guard 后到期时间应重算");

        // 显式清空 guard → 到期时间也清空
        apply_credential("123456", Some("sid-1"), Some(""), None, "manual", true).unwrap();
        let acc = find_account("123456").unwrap();
        assert!(acc["sid_guard"].is_null());
        assert!(acc["session_expire_at"].is_null());
    }

    #[test]
    fn 抓包回写按抓包文件的_uid_定位且不自动建号() {
        let _iso = Isolated::new("capture");

        // 账号池里有 A、B 两个账号
        upsert_account("111", None, None, true).unwrap();
        upsert_account("222", None, None, true).unwrap();

        // 抓包文件声明 uid=222
        save_captured(&json!({
            "session_id": "captured-sid",
            "sid_guard": "captured-guard",
            "ttwid": "captured-tw",
            "uid": "222",
        }))
        .unwrap();

        let applied = auto_apply_captured().unwrap().expect("应回写成功");
        assert_eq!(applied["userId"], "222");
        // A 必须完全没被碰过（跨账号污染回归测试）
        let a = find_account("111").unwrap();
        assert!(a.get("session_id").is_none() || a["session_id"].is_null());
        assert_eq!(find_account("222").unwrap()["session_id"], "captured-sid");

        // 幂等：内容没变时不再写盘
        assert!(
            auto_apply_captured().unwrap().is_none(),
            "内容未变化时必须跳过写入"
        );

        // 抓包 uid 不在池中 → 不建号
        save_captured(&json!({
            "session_id": "other-sid",
            "uid": "333",
        }))
        .unwrap();
        assert!(auto_apply_captured().unwrap().is_none());
        assert!(find_account("333").is_none(), "不得自动创建账号");
    }

    #[test]
    fn 抓包缺_uid_时不回写() {
        let _iso = Isolated::new("nouid");
        upsert_account("111", None, None, true).unwrap();
        save_captured(&json!({"session_id": "sid-only"})).unwrap();
        assert!(
            auto_apply_captured().unwrap().is_none(),
            "没有 multi_sids 就无法可靠归属，宁可不写"
        );
    }
}
