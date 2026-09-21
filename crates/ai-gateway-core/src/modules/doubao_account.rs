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
/// 四条硬性约束（都来自实测踩坑）：
/// 1. **目标账号取抓包文件自己的 uid** —— 用探测链定位会让两个账号拿到同一凭证
/// 2. **绝不自动建号** —— 网页版/其他字节应用的 cookie 会污染账号池
/// 3. **幂等** —— 写入的字段都没变时不写盘，
///    避免每 20 秒的轮询把文件时间戳刷得毫无意义
/// 4. **已有凭证的账号不被覆盖** —— 映射里的 sid 可能比池里的旧
///
/// ## 三条取值路径（按可靠性从高到低）
///
/// - **精确**：抓包文件带 `uid` 且该 uid 在池中 → 直接回写（原路径）
/// - **映射**：抓包文件带 `sids`（`multi_sids` 全量映射）→ 池内属于该映射的账号
///   各自补上**自己那一条** sessionid。一个豆包客户端可同时挂多个账号，
///   而一次请求只带当前那一个 sessionid —— 映射里其余项本来就在 cookie 里，
///   不补等于白丢
/// - **唯一候选**：连 `multi_sids` 都没有时，若池中**恰好只有一个**账号缺凭证，
///   认它。候选为 0 或 ≥2 都拒绝 —— 归属不明时宁可不写（跨账号污染事故的教训）
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
    let sid_guard = captured.get("sid_guard").and_then(Value::as_str);
    let ttwid = captured.get("ttwid").and_then(Value::as_str);

    // 路径 1：精确匹配（`uid` 来自 `multi_sids` 里与本次 sessionid 相等的那一项）
    if !uid.is_empty() && ensure_uid_safe(uid).is_ok() {
        if let Some(existing) = find_account(uid) {
            if !credential_unchanged(&existing, Some(session_id), sid_guard, ttwid) {
                let updated =
                    apply_credential(uid, Some(session_id), sid_guard, ttwid, "proxy", false)?;
                // **不能在这里 return**：这一份抓包同时带着 multi_sids 映射，
                // 同机其余账号的 sessionid 就在映射里。只回写当前这一个就等于
                // 把它们白丢（「读得到却没填进去」）。先落当前账号，再继续走映射路径。
                let current = account_view(&updated);
                return Ok(Some(
                    apply_sids_map(&captured, sid_guard, ttwid)?.unwrap_or(current),
                ));
            }
            // 该账号无变化，但仍可能有别的账号能从映射里补全 —— 不 return，
            // 继续走下面两条路径（映射路径是幂等的，重复调用无副作用）
        }
    }

    // 路径 2：`multi_sids` 全量映射，逐个补全池内账号
    if let Some(applied) = apply_sids_map(&captured, sid_guard, ttwid)? {
        return Ok(Some(applied));
    }

    // 路径 3：uid 缺失（或不在池中）时的唯一候选推断。
    // 只在**完全没有** uid 线索时才用 —— 有 uid 却不在池中意味着这是别人的
    // 会话（网页版/其他字节应用），推断会把它错记到池内唯一那个账号上。
    if uid.is_empty() {
        if let Some(applied) = apply_unique_candidate(session_id, sid_guard, ttwid)? {
            return Ok(Some(applied));
        }
    }

    Ok(None)
}

/// 判断账号的凭证字段是否与将写入的值完全一致（幂等比对）。
///
/// 空/缺失的入参视为「不写入」，因此不算变化 —— 与 [`apply_credential`]
/// 的「None 不覆盖」语义一致。
fn credential_unchanged(
    existing: &Value,
    session_id: Option<&str>,
    sid_guard: Option<&str>,
    ttwid: Option<&str>,
) -> bool {
    let same = |key: &str, incoming: Option<&str>| -> bool {
        let current = existing.get(key).and_then(Value::as_str).unwrap_or("");
        match incoming.map(str::trim) {
            None | Some("") => true,
            Some(v) => v == current,
        }
    };
    same("session_id", session_id) && same("sid_guard", sid_guard) && same("ttwid", ttwid)
}

/// 路径 2：按 `sids` 映射给池内账号补全各自缺失的凭证。
///
/// 只补**当前没有有效 sessionid** 的账号：池里的凭证可能比映射里的更新
/// （映射是本次抓包的快照），用它覆盖等于把刚续期好的凭证退回旧值。
///
/// `ttwid` / `sid_guard` 是设备级/会话级字段，对同机全部账号都适用，
/// 因此对所有从映射命中的账号都写（沿用「显式给值才覆盖」语义）。
///
/// 返回第一个真正发生变化的账号视图；无任何变化返回 `None`。
fn apply_sids_map(
    captured: &Value,
    sid_guard: Option<&str>,
    ttwid: Option<&str>,
) -> Result<Option<Value>, String> {
    let Some(map) = captured.get("sids").and_then(Value::as_object) else {
        return Ok(None);
    };
    if map.is_empty() {
        return Ok(None);
    }
    let mut first: Option<Value> = None;
    for (uid, sid) in map {
        let uid = uid.trim();
        let sid = sid.as_str().map(str::trim).unwrap_or("");
        if sid.is_empty() || ensure_uid_safe(uid).is_err() {
            continue;
        }
        let Some(existing) = find_account(uid) else {
            continue; // 不自动建号
        };
        // 该账号已有凭证 → 只补它缺的设备级字段，绝不改 sessionid
        let has_credential = existing
            .get("session_id")
            .and_then(Value::as_str)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let incoming_sid = if has_credential { None } else { Some(sid) };
        if credential_unchanged(&existing, incoming_sid, sid_guard, ttwid) {
            continue;
        }
        let updated = apply_credential(uid, incoming_sid, sid_guard, ttwid, "proxy", false)?;
        if first.is_none() {
            first = Some(account_view(&updated));
        }
    }
    Ok(first)
}

/// 路径 3：uid 缺失时的唯一候选推断。
///
/// `uid` 来自 `multi_sids`，不是每个请求都带。而当池中**恰好只有一个**账号
/// 还没有凭证时，本次抓到的 sessionid 只可能是它的 —— 这属于「读得到却填不上」
/// 的典型场景，不推断就等于要求用户手贴一个本机已有的值。
///
/// **候选数 ≥2 时必须拒绝**：这时无法判断 sessionid 属于谁，
/// 猜错会把 A 的凭证写到 B 名下（[`crate::modules::doubao_account`] 文档记录的
/// 账号 908/232 跨账号污染事故）。
///
/// `session_source` 记为 `proxy_inferred` 而非 `proxy`，让用户能分辨
/// 「这条凭证是推断归属的」，而不是服务端直接标明的。
fn apply_unique_candidate(
    session_id: &str,
    sid_guard: Option<&str>,
    ttwid: Option<&str>,
) -> Result<Option<Value>, String> {
    let candidates: Vec<String> = load_accounts()
        .iter()
        .filter(|a| {
            a.get("session_id")
                .and_then(Value::as_str)
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
        })
        .filter_map(|a| a.get("user_id").and_then(Value::as_str).map(str::to_string))
        .collect();
    if candidates.len() != 1 {
        return Ok(None);
    }
    let uid = &candidates[0];
    let Some(existing) = find_account(uid) else {
        return Ok(None);
    };
    if credential_unchanged(&existing, Some(session_id), sid_guard, ttwid) {
        return Ok(None);
    }
    let updated = apply_credential(
        uid,
        Some(session_id),
        sid_guard,
        ttwid,
        "proxy_inferred",
        false,
    )?;
    Ok(Some(account_view(&updated)))
}

/// 凭证归属推断的当前状态（供 [`crate::modules::doubao_session::diagnose`] 解释
/// 「为什么抓到了凭证却没填上」）。
///
/// 返回 `(缺少凭证的账号数, 是否可自动推断)`。
pub fn credential_inference_state() -> (usize, bool) {
    let missing = load_accounts()
        .iter()
        .filter(|a| {
            a.get("session_id")
                .and_then(Value::as_str)
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
        })
        .count();
    (missing, missing == 1)
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
    fn 抓包缺_uid_但有唯一候选时推断归属() {
        let _iso = Isolated::new("nouid-uniq");
        // 池中只有一个账号，且它没有凭证 → 本次抓到的 sid 只可能是它的
        upsert_account("111", None, None, true).unwrap();
        save_captured(&json!({
            "session_id": "sid-only",
            "sid_guard": "guard-only",
            "ttwid": "tw-only",
        }))
        .unwrap();
        let applied = auto_apply_captured().unwrap().expect("唯一候选应被推断");
        assert_eq!(applied["userId"], "111");
        let acc = find_account("111").unwrap();
        assert_eq!(acc["session_id"], "sid-only");
        assert_eq!(acc["ttwid"], "tw-only");
        assert_eq!(
            acc["session_source"], "proxy_inferred",
            "推断归属必须与「服务端直接标明」区分开，便于排查"
        );
        // 幂等
        assert!(auto_apply_captured().unwrap().is_none());
    }

    /// 回归：候选数 ≥2 时**绝不**推断 —— 归属不明时猜错就是跨账号凭证污染。
    #[test]
    fn 抓包缺_uid_且多候选时拒绝推断() {
        let _iso = Isolated::new("nouid-multi");
        upsert_account("111", None, None, true).unwrap();
        upsert_account("222", None, None, true).unwrap();
        save_captured(&json!({"session_id": "sid-only", "ttwid": "tw-only"})).unwrap();
        assert!(
            auto_apply_captured().unwrap().is_none(),
            "两个候选无法区分，必须拒绝写入"
        );
        for uid in ["111", "222"] {
            let acc = find_account(uid).unwrap();
            assert!(
                acc.get("session_id").is_none() || acc["session_id"].is_null(),
                "账号 {uid} 不得被写入不属于它的凭证"
            );
            assert!(acc.get("ttwid").is_none(), "ttwid 也不得写入");
        }
        // 诊断应把「有几个候选」摆给用户看，而不是让他反复重试
        assert_eq!(credential_inference_state(), (2, false));
    }

    /// 映射路径：一个客户端挂多个账号时，一次抓包要把**所有**已知账号都补上。
    #[test]
    fn 抓包映射为同机多账号各自补全() {
        let _iso = Isolated::new("sids-map");
        upsert_account("111", None, None, true).unwrap();
        upsert_account("222", None, None, true).unwrap();
        // 本次会话是 222，multi_sids 里同时带着 111
        save_captured(&json!({
            "session_id": "sidB",
            "sid_guard": "guard-x",
            "ttwid": "tw-x",
            "uid": "222",
            "sids": {"111": "sidA", "222": "sidB"},
        }))
        .unwrap();

        assert!(auto_apply_captured().unwrap().is_some(), "应至少回写一个账号");
        let a = find_account("111").unwrap();
        let b = find_account("222").unwrap();
        assert_eq!(a["session_id"], "sidA", "111 必须拿到**自己那条** sid");
        assert_eq!(b["session_id"], "sidB");
        assert_eq!(a["ttwid"], "tw-x", "ttwid 是设备级字段，同机账号都该补上");
        assert_eq!(b["ttwid"], "tw-x");
        assert_eq!(a["session_source"], "proxy");
        // 幂等：全部无变化
        assert!(auto_apply_captured().unwrap().is_none());
    }

    /// 映射路径**不得覆盖**已有凭证的账号：映射是本次抓包快照，
    /// 池里的可能是刚续期好的更新的凭证。
    #[test]
    fn 抓包映射不覆盖已有凭证() {
        let _iso = Isolated::new("sids-nocover");
        upsert_account("111", None, None, true).unwrap();
        apply_credential("111", Some("fresh-sid"), None, None, "manual", true).unwrap();

        save_captured(&json!({
            "session_id": "sidB",
            "uid": "222", // 不在池中，精确路径落空
            "sids": {"111": "stale-sid"},
        }))
        .unwrap();
        assert!(
            auto_apply_captured().unwrap().is_none(),
            "111 已有凭证，映射不得改动任何字段"
        );
        assert_eq!(
            find_account("111").unwrap()["session_id"],
            "fresh-sid",
            "已有凭证绝不能被映射里的旧值退回"
        );
    }

    /// 映射里出现池外账号 → 不建号（沿用既有防污染约束）。
    #[test]
    fn 抓包映射不自动建号() {
        let _iso = Isolated::new("sids-nocreate");
        upsert_account("111", None, None, true).unwrap();
        save_captured(&json!({
            "session_id": "sidB",
            "uid": "222",
            "sids": {"111": "sidA", "999": "sidZ"},
        }))
        .unwrap();
        auto_apply_captured().unwrap();
        assert!(find_account("999").is_none(), "池外账号不得自动创建");
        assert_eq!(find_account("111").unwrap()["session_id"], "sidA");
    }

    /// 抓包 uid 在池中、映射为空时，精确路径照常工作（老抓包文件的兼容性）。
    #[test]
    fn 抓包无映射字段时退化为精确路径() {
        let _iso = Isolated::new("sids-absent");
        upsert_account("111", None, None, true).unwrap();
        save_captured(&json!({
            "session_id": "sidA",
            "uid": "111",
        }))
        .unwrap();
        let applied = auto_apply_captured().unwrap().expect("应回写成功");
        assert_eq!(applied["userId"], "111");
        assert_eq!(applied["sessionSource"], "proxy");
    }
}
