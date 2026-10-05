//! Trae 账号库：存储、JWT 解析、设备指纹绑定。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/models.rs`（`RawAccount`）+
//! `src-tauri/src/jwt.rs` + `commands/accounts.rs`（设备派生部分）。
//!
//! ## 与 WorkBuddy 账号库分开存
//!
//! Trae 账号的凭证形态完全不同（`Cloud-IDE-JWT` + `refresh_token`，而非
//! WorkBuddy 的 `access_token` + `domain`），且**两个 Trae 应用共用同一份账号库**
//! （Trae Work 与 Trae 只是同一账号的两套登录态，不是两批账号）。
//! 混进 WorkBuddy 的 `accounts.json` 会让两边的身份匹配、导出、网关凭证生成
//! 全部需要按类型分支，得不偿失。

use serde_json::{json, Value};

use crate::modules::config;
use crate::modules::trae_device;

/// JWT 解析结果（不校验签名，仅本地展示与路由用途）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JwtInfo {
    pub user_id: Option<String>,
    /// 剩余有效小时数。
    pub exp_hours: Option<f64>,
    pub exp_timestamp: Option<i64>,
    /// token 的 `iss` / `issuer` 声明。
    ///
    /// ★ 这是**区域判定最权威的线索**（见 [`crate::modules::trae_realm`]）：
    /// issuer 是账号的属性，而「当前选中哪个区域」只是路由选择。
    /// 拿不到 issuer 时区域判定会退化成兜底猜测，可能把国际账号的
    /// token 刷新发去中国 auth host（必然失败）。
    pub issuer: Option<String>,
}

impl JwtInfo {
    /// 由剩余小时数推导状态：`>24h` ok / `>0` warn / 已过期 expired / 未知 unknown。
    pub fn status(&self) -> &'static str {
        match self.exp_hours {
            Some(h) if h > 24.0 => "ok",
            Some(h) if h > 0.0 => "warn",
            Some(_) => "expired",
            None => "unknown",
        }
    }
}

/// 解析 JWT，提取 uid 与过期时间。
///
/// 兼容三种前缀（`Cloud-IDE-JWT ` / `Bearer ` / 裸 token）：上游在不同路径下
/// 观察到过不同前缀，用户手动粘贴时更是三种都可能出现。
pub fn parse_jwt(jwt_full: &str) -> JwtInfo {
    use base64::Engine;

    let trimmed = jwt_full.trim();
    let token = trimmed
        .strip_prefix("Cloud-IDE-JWT ")
        .or_else(|| trimmed.strip_prefix("Bearer "))
        .unwrap_or(trimmed)
        .trim();

    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return JwtInfo::default();
    }
    // JWT payload 是 base64url 无填充；先去掉可能的残余 '=' 再解码
    let payload_b64 = parts[1].trim_end_matches('=');
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload_b64) else {
        return JwtInfo::default();
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&bytes) else {
        return JwtInfo::default();
    };

    // uid 优先取 data.id（Cloud-IDE id 空间），回退 auth_id / sub
    let user_id = payload
        .get("data")
        .and_then(|d| d.get("id"))
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
        })
        .or_else(|| {
            payload
                .get("auth_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| payload.get("sub").and_then(Value::as_str).map(str::to_string));

    // exp 可能是整数、浮点或数字字符串
    let exp_timestamp = payload.get("exp").and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_f64().map(|f| f as i64))
            .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
    });
    let exp_hours = exp_timestamp.map(|exp| (exp - chrono::Utc::now().timestamp()) as f64 / 3600.0);

    // issuer：`iss` 是标准声明，`issuer` 是上游实际用过的变体。
    // 只做字符串提取，**不校验签名** —— 本函数从不做信任判断，
    // issuer 仅用于区域路由（发错 host 会失败，不会泄露任何东西）。
    let issuer = payload
        .get("iss")
        .or_else(|| payload.get("issuer"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    JwtInfo {
        user_id,
        exp_hours,
        exp_timestamp,
        issuer,
    }
}

/// 把裸 token 规范成 `Cloud-IDE-JWT <token>` 形态（幂等）。
pub fn normalize_jwt(jwt: &str) -> String {
    let trimmed = jwt.trim();
    if trimmed.starts_with("Cloud-IDE-JWT ") || trimmed.starts_with("Bearer ") {
        trimmed.to_string()
    } else {
        format!("Cloud-IDE-JWT {trimmed}")
    }
}

/// 账号库文件（`<store_dir>/trae_accounts.json`）。
pub fn accounts_file() -> std::path::PathBuf {
    config::store_dir().join("trae_accounts.json")
}

/// 读取全部 Trae 账号（缺失或损坏返回空列表）。
pub fn load_accounts() -> Vec<Value> {
    std::fs::read_to_string(accounts_file())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| {
            v.get("accounts")
                .and_then(Value::as_array)
                .cloned()
                .or_else(|| v.as_array().cloned())
        })
        .unwrap_or_default()
}

/// 写回账号库（原子写，保持 `{"accounts": [...]}` 结构）。
pub fn save_accounts(accounts: &[Value]) -> std::io::Result<()> {
    std::fs::create_dir_all(config::store_dir())?;
    let content = serde_json::to_string_pretty(&json!({ "accounts": accounts })).unwrap_or_default();
    config::atomic_write(&accounts_file(), &content)
}

/// 按 uid 查找账号。
pub fn find_account(uid: &str) -> Option<Value> {
    let uid = uid.trim();
    load_accounts()
        .into_iter()
        .find(|a| a.get("user_id").and_then(Value::as_str) == Some(uid))
}

/// 账号展示名（`name` → `账号_<uid 前 8 位>`）。
pub fn display_name(acc: &Value) -> String {
    acc.get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            let uid = acc.get("user_id").and_then(Value::as_str).unwrap_or("");
            format!("账号_{}", uid.chars().take(8).collect::<String>())
        })
}

/// 新增或更新账号（按 uid 去重）。
///
/// 更新已存在的账号时：刷新 `jwt` / `refresh_token`，并把连续刷新失败计数清零 ——
/// 用户重新登录意味着凭证已换新，旧的失败历史不应继续影响调度决策。
pub fn upsert_account(
    uid: &str,
    name: Option<&str>,
    jwt: &str,
    refresh_token: Option<&str>,
) -> Result<Value, String> {
    let uid = uid.trim();
    if uid.is_empty() {
        return Err("缺少账号标识（uid）".to_string());
    }
    if jwt.trim().is_empty() {
        return Err("缺少 JWT".to_string());
    }
    let mut accounts = load_accounts();
    let now = config::utc_iso();
    let normalized = normalize_jwt(jwt);

    if let Some(existing) = accounts
        .iter_mut()
        .find(|a| a.get("user_id").and_then(Value::as_str) == Some(uid))
    {
        let obj = existing
            .as_object_mut()
            .ok_or_else(|| "账号记录格式异常".to_string())?;
        obj.insert("jwt".to_string(), json!(normalized));
        if let Some(rt) = refresh_token.map(str::trim).filter(|s| !s.is_empty()) {
            obj.insert("refresh_token".to_string(), json!(rt));
        }
        if let Some(n) = name.map(str::trim).filter(|s| !s.is_empty()) {
            obj.insert("name".to_string(), json!(n));
        }
        obj.insert("updated_at".to_string(), json!(now));
        obj.insert("refresh_token_fails".to_string(), json!(0));
        obj.insert("refresh_token_invalid".to_string(), json!(false));
        let updated = existing.clone();
        save_accounts(&accounts).map_err(|e| e.to_string())?;
        return Ok(updated);
    }

    let acc = json!({
        "user_id": uid,
        "name": name.map(str::trim).filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("账号_{}", uid.chars().take(8).collect::<String>())),
        "jwt": normalized,
        "refresh_token": refresh_token.map(str::trim).filter(|s| !s.is_empty()),
        "added_at": now,
        "updated_at": now,
        "refresh_token_fails": 0,
        "refresh_token_invalid": false,
    });
    accounts.push(acc.clone());
    save_accounts(&accounts).map_err(|e| e.to_string())?;
    Ok(acc)
}

/// 账号名是否是**占位名**（自动生成、没有信息量）。
///
/// 两类占位名：
/// - `auto_<uid 前 8 位>` —— 代理/本地捕获自动追加账号时的命名
/// - `账号_<uid 前 8 位>` —— [`upsert_account`] 未给名字时的兜底
///
/// 只有占位名才允许被自动改名。用户手动取的名字（哪怕恰好叫 "auto_xxx" 之外的任何值）
/// 一律保留 —— 自动改名覆盖用户输入是不可接受的。
pub fn is_placeholder_name(name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() {
        return true;
    }
    if let Some(rest) = name.strip_prefix("auto_") {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric());
    }
    if let Some(rest) = name.strip_prefix("账号_") {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric());
    }
    false
}

/// 仅在账号名还是占位名时改名为 `name`（用户自定义名绝不覆盖）。
///
/// 用途：「发现本机账号」已经读到了套餐身份（如 `Pro`），但账号是捕获流程
/// 自动建的、名字还叫 `auto_<uid8>` —— 把这个信息填进去，用户一眼能认出是哪个号。
///
/// 返回是否真的改了名。
pub fn set_name_if_placeholder(uid: &str, name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() {
        return false;
    }
    let mut accounts = load_accounts();
    let Some(acc) = accounts
        .iter_mut()
        .find(|a| a.get("user_id").and_then(Value::as_str) == Some(uid.trim()))
    else {
        return false;
    };
    let current = acc.get("name").and_then(Value::as_str).unwrap_or("");
    if !is_placeholder_name(current) {
        return false;
    }
    if current == name {
        return false;
    }
    let Some(obj) = acc.as_object_mut() else {
        return false;
    };
    obj.insert("name".to_string(), json!(name));
    obj.insert("updated_at".to_string(), json!(config::utc_iso()));
    save_accounts(&accounts).is_ok()
}

/// 删除账号。
pub fn delete_account(uid: &str) -> Result<(), String> {
    config::ensure_uid_safe(uid)?;
    let uid = uid.trim();
    let mut accounts = load_accounts();
    let before = accounts.len();
    accounts.retain(|a| a.get("user_id").and_then(Value::as_str) != Some(uid));
    if accounts.len() == before {
        return Err("账号不存在".to_string());
    }
    save_accounts(&accounts).map_err(|e| e.to_string())
}

/// 更新账号的 JWT（刷新流程用）。
pub fn update_jwt(uid: &str, jwt: &str) -> Result<(), String> {
    config::ensure_uid_safe(uid)?;
    let uid = uid.trim();
    let mut accounts = load_accounts();
    let acc = accounts
        .iter_mut()
        .find(|a| a.get("user_id").and_then(Value::as_str) == Some(uid))
        .ok_or_else(|| "账号不存在".to_string())?;
    let obj = acc
        .as_object_mut()
        .ok_or_else(|| "账号记录格式异常".to_string())?;
    obj.insert("jwt".to_string(), json!(normalize_jwt(jwt)));
    obj.insert("updated_at".to_string(), json!(config::utc_iso()));
    save_accounts(&accounts).map_err(|e| e.to_string())
}

/// 重命名账号（只改昵称，不动凭证与设备指纹）。
///
/// 与 [`update_jwt`] 分开：界面上的「编辑」既可能只改昵称，也可能连 JWT 一起换。
/// 合在一个函数里的话，只改昵称的调用也不得不传 JWT，容易把空串写进去覆盖掉
/// 现有登录态。
pub fn rename_account(uid: &str, name: &str) -> Result<(), String> {
    config::ensure_uid_safe(uid)?;
    let uid = uid.trim();
    let name = name.trim();
    if name.is_empty() {
        return Err("昵称不能为空".to_string());
    }
    let mut accounts = load_accounts();
    let acc = accounts
        .iter_mut()
        .find(|a| a.get("user_id").and_then(Value::as_str) == Some(uid))
        .ok_or_else(|| "账号不存在".to_string())?;
    let obj = acc
        .as_object_mut()
        .ok_or_else(|| "账号记录格式异常".to_string())?;
    obj.insert("name".to_string(), json!(name));
    obj.insert("updated_at".to_string(), json!(config::utc_iso()));
    save_accounts(&accounts).map_err(|e| e.to_string())
}

/// 单独更新账号的 `refresh_token`（编辑弹窗里手填续期凭证）。
pub fn set_refresh_token(uid: &str, refresh_token: &str) -> Result<(), String> {
    config::ensure_uid_safe(uid)?;
    let uid = uid.trim();
    let rt = refresh_token.trim();
    if rt.is_empty() {
        return Err("refresh_token 不能为空".to_string());
    }
    let mut accounts = load_accounts();
    let acc = accounts
        .iter_mut()
        .find(|a| a.get("user_id").and_then(Value::as_str) == Some(uid))
        .ok_or_else(|| "账号不存在".to_string())?;
    let obj = acc
        .as_object_mut()
        .ok_or_else(|| "账号记录格式异常".to_string())?;
    obj.insert("refresh_token".to_string(), json!(rt));
    // 手填意味着用户认为它是好的：清掉失效标记，否则「可续期」永远显示为假
    obj.insert("refresh_token_invalid".to_string(), json!(false));
    obj.insert("refresh_token_fails".to_string(), json!(0));
    obj.insert("updated_at".to_string(), json!(config::utc_iso()));
    save_accounts(&accounts).map_err(|e| e.to_string())
}

/// 单独设置账号的「是否参加兼容网关路由」（账号级开关）。
///
/// ★ 语义 = 网关侧 `no_route`（业主原话「关闭平台只是不参与兼容网关,并不是不执行
/// 任务请注意」，账号级同理）：只把该账号从**选号**里排除；签到 / 上报 / 成长任务 /
/// 额度巡检**照常运行**。
///
/// 落盘方式与 WorkBuddy / Qoder / ZCode **完全同款**：写 `disabled` 字段，
/// 由 `export_accounts_to_gateway` 在导出凭证时映射成 `account.no_route = true`
/// （见 `build_credential_text`）。这里**不**碰网关的凭证文件 —— 那是导出的职责，
/// 两者分开才能保证「界面改完 → 导出 → 网关重扫」这条链只有一个出口。
///
/// `false` 时把键置为 `false` 而不是删掉：与 WorkBuddy 的账号库一致
///（那里也是显式写 false），这样「用户手动打开过」与「从没配过」在文件里可分辨。
pub fn set_disabled(uid: &str, disabled: bool) -> Result<(), String> {
    config::ensure_uid_safe(uid)?;
    let uid = uid.trim();
    let mut accounts = load_accounts();
    let acc = accounts
        .iter_mut()
        .find(|a| a.get("user_id").and_then(Value::as_str) == Some(uid))
        .ok_or_else(|| "账号不存在".to_string())?;
    let obj = acc
        .as_object_mut()
        .ok_or_else(|| "账号记录格式异常".to_string())?;
    obj.insert("disabled".to_string(), json!(disabled));
    obj.insert("updated_at".to_string(), json!(config::utc_iso()));
    save_accounts(&accounts).map_err(|e| e.to_string())
}

/// 读取账号的**完整** JWT（仅供单账号查看/编辑弹窗回填）。
///
/// 与 [`account_meta`] 的区别：列表视图一律脱敏，只有用户主动点「查看」时才
/// 返回明文。两者必须分开，否则脱敏就成了摆设。
pub fn account_jwt(uid: &str) -> Result<String, String> {
    config::ensure_uid_safe(uid)?;
    find_account(uid.trim())
        .and_then(|a| a.get("jwt").and_then(Value::as_str).map(str::to_string))
        .filter(|j| !j.trim().is_empty())
        .ok_or_else(|| "账号不存在或没有 JWT".to_string())
}

/// 标记 refresh_token 连续刷新失败（达到阈值时判定失效）。
///
/// 返回 `(连续失败次数, 是否已判定失效)`。
///
/// **只有 `invalid=true`（服务端明确拒绝）才立即置失效**；瞬时失败（网络抖动、
/// 代理未启动）只递增计数，由 [`crate::modules::trae_refresh`] 在连续 3 次后兜底。
/// 早期版本把两类混为一谈，一次网络异常就能把整批账号永久标成失效并移出账号池。
pub fn note_refresh_failure(uid: &str, invalid: bool) -> (i64, bool) {
    let mut accounts = load_accounts();
    let Some(acc) = accounts
        .iter_mut()
        .find(|a| a.get("user_id").and_then(Value::as_str) == Some(uid.trim()))
    else {
        return (0, false);
    };
    let Some(obj) = acc.as_object_mut() else {
        return (0, false);
    };
    let fails = obj
        .get("refresh_token_fails")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        + 1;
    obj.insert("refresh_token_fails".to_string(), json!(fails));
    // 连续 3 次瞬时失败也兜底置失效：账号确实已经换不回来了，
    // 继续留在池里只会每次保活都白打一次网络请求。
    let invalid_now = invalid || fails >= REFRESH_FAILS_TO_INVALID;
    if invalid_now {
        obj.insert("refresh_token_invalid".to_string(), json!(true));
    }
    let _ = save_accounts(&accounts);
    (fails, invalid_now)
}

/// 瞬时刷新失败达到该次数后兜底判定 `refresh_token` 失效。
pub const REFRESH_FAILS_TO_INVALID: i64 = 3;

/// 刷新成功的收尾：写入新 access token（与轮换后的 refresh_token），
/// 清零失败计数并解除失效标记。
///
/// **成功必须解冻**：只写 token 不清标记的话，账号仍带着 `refresh_token_invalid=true`，
/// 调度层会继续跳过它 —— 表现为「刷新成功了但账号还是不用」，排查成本极高。
pub fn mark_refresh_success(
    uid: &str,
    new_refresh_token: Option<&str>,
    rt_expires_at: Option<i64>,
) -> Result<Value, String> {
    let mut accounts = load_accounts();
    let acc = accounts
        .iter_mut()
        .find(|a| a.get("user_id").and_then(Value::as_str) == Some(uid.trim()))
        .ok_or_else(|| "账号不存在".to_string())?;
    let obj = acc
        .as_object_mut()
        .ok_or_else(|| "账号记录格式异常".to_string())?;
    obj.insert("refresh_token_fails".to_string(), json!(0));
    obj.insert("refresh_token_invalid".to_string(), json!(false));
    if let Some(rt) = new_refresh_token.map(str::trim).filter(|s| !s.is_empty()) {
        obj.insert("refresh_token".to_string(), json!(rt));
    }
    if let Some(exp) = rt_expires_at {
        obj.insert("refresh_token_expires_at".to_string(), json!(exp));
    }
    obj.insert("refreshed_at".to_string(), json!(config::utc_iso()));
    let updated = acc.clone();
    save_accounts(&accounts).map_err(|e| e.to_string())?;
    Ok(updated)
}

/// 由账号记录判定其所属区域（权威方向：凭据属性优先）。
///
/// 线索来源与优先级（详见 [`crate::modules::trae_realm`] 的模块说明）：
/// 1. 账号记录里显式写入的 `_edition` / `realm`
/// 2. JWT 的 `iss` / `issuer` 声明 —— **最权威**
/// 3. 账号记录里记录的 `userRegion`
/// 4. 兜底（此时 `evidence` 为 `Default`，调用方应据此提示用户）
///
/// 为什么必须这样排序：issuer 与 auth host 是**账号的属性**，而「当前选中
/// 哪个区域」只是路由选择。若先按选中区域贴标签，每个国际账号都会被静默
/// 误标成国内，随后它的 token 刷新被发去中国 auth host 而必然失败。
pub fn account_realm(acc: &Value) -> crate::modules::trae_realm::RealmGuess {
    use crate::modules::trae_realm::{realm_from_credential, CredentialHint};

    let explicit = acc
        .get("_edition")
        .or_else(|| acc.get("realm"))
        .and_then(Value::as_str);
    let jwt = acc.get("jwt").and_then(Value::as_str).unwrap_or("");
    let info = parse_jwt(jwt);
    let region = acc
        .get("user_region")
        .or_else(|| acc.get("userRegion"))
        .and_then(|v| {
            // `userRegion` 可能是对象（取 `region`）或直接是字符串
            v.get("region")
                .and_then(Value::as_str)
                .or_else(|| v.as_str())
        });

    realm_from_credential(&CredentialHint {
        explicit,
        // 账号记录里若记了 host 也一并用上（老记录可能没有）
        host: acc.get("auth_host").and_then(Value::as_str),
        issuer: info.issuer.as_deref(),
        region,
    })
}

/// 账号的展示元数据（**不泄露 JWT**）。
///
/// JWT 绝不离开后端：界面只需要知道「有效 / 即将过期 / 已过期」与 uid。
pub fn account_meta(acc: &Value) -> Value {
    let jwt = acc.get("jwt").and_then(Value::as_str).unwrap_or("");
    let info = parse_jwt(jwt);
    let uid = acc
        .get("user_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let device = trae_device::derive_device(&uid);
    let realm = account_realm(acc);
    json!({
        "userId": uid,
        "name": display_name(acc),
        "addedAt": acc.get("added_at"),
        "updatedAt": acc.get("updated_at"),
        "jwtStatus": info.status(),
        "jwtExpHours": info.exp_hours,
        "jwtExpTimestamp": info.exp_timestamp,
        // 区域与判定依据：`realmDetected=false` 表示这是**兜底猜的**，
        // 界面应据此提示用户（而不是假装已经知道该账号属于哪个区域）。
        "realm": realm.realm.as_str(),
        "realmLabel": realm.realm.label(),
        "realmEvidence": realm.evidence.as_str(),
        "realmDetected": realm.evidence.is_detected(),
        "hasRefreshToken": acc
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "refreshTokenInvalid": acc
            .get("refresh_token_invalid")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "refreshTokenFails": acc
            .get("refresh_token_fails")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        "refreshTokenExpiresAt": acc
            .get("refresh_token_expires_at")
            .and_then(Value::as_i64),
        "refreshedAt": acc.get("refreshed_at"),
        "canRefresh": acc
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
            && !acc
                .get("refresh_token_invalid")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        // 设备指纹脱敏展示：完整值只在后端注入请求头时使用
        "deviceIdMasked": mask_tail(&device.device_id, 4),
        // 套餐身份与积分缓存：来自 `trae_credits` 的付费身份缓存。
        // 缺失时给 null 而**不是 0** —— 「不知道」与「确实是 0 积分」必须能区分，
        // 否则界面会把没查过的账号显示成「0 积分」，用户以为积分被清零了。
        "payIdentity": acc.get("pay_identity").cloned().unwrap_or(Value::Null),
        "payExpireAt": acc.get("pay_expire_at").cloned().unwrap_or(Value::Null),
        "creditsTotal": acc.get("credits_total").cloned().unwrap_or(Value::Null),
        "creditsUpdatedAt": acc.get("credits_updated_at").cloned().unwrap_or(Value::Null),
        "group": acc.get("group_id").cloned().unwrap_or(Value::Null),
        // 「是否参加兼容网关路由」账号级开关（缺省 false = 参加）。
        // ★ 与网关侧 `no_route` 同义：不选号，但签到/上报/成长任务照跑。
        "disabled": acc
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// 脱敏：只保留末 `keep` 位。
fn mask_tail(value: &str, keep: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= keep {
        return "*".repeat(chars.len());
    }
    let masked = "*".repeat(chars.len() - keep);
    format!("{masked}{}", chars[chars.len() - keep..].iter().collect::<String>())
}

// ---------------------------------------------------------------------------
// 凭证导出（账号库 -> 网关可读的凭证目录）
// ---------------------------------------------------------------------------
//
// 网关的账号池是**扫描凭证目录**建立的，宿主的 trae_accounts.json 它根本不读
//（与 WorkBuddy / Qoder / ZCode 完全同款，见 zcode_account::sync_no_route_to_credential
// 的注释）。缺了这一段，界面里登录成功的账号永远不会参与选号 —— 症状是
//「Trae 登录成功了，但请求永远只走 WorkBuddy」。
//
// ★ 与 WorkBuddy 导出链路的关键差异：这里**不做 needs_relogin 过滤**。
//   needs_relogin 是 WorkBuddy 刷新链的核心概念（refresh token 换不回 token 时把
//   账号移出），Trae 侧对应的概念是 JWT 的过期时间，且网关自己会在
//   products/trae/client.rs 里按 LAZY_REFRESH_MIN_SECS 懒刷新。在这里提前滤掉，
//   只会让「刚登录、字段还没写全」的账号凭空消失。

/// 网关凭证文件名前缀（与网关侧 products/trae/cred.rs 的 CRED_PREFIX 一致）。
pub const CRED_PREFIX: &str = "trae-";

/// 账号库所在目录。Trae 的账号库就在 store_dir 根下，**没有子目录**。
///
/// 与 accounts_file 同源（config::store_dir() 拼 trae_accounts.json）——
/// 两者一旦分叉，就会出现「保存成功但导出读的是另一个文件」。
pub fn store_dir() -> std::path::PathBuf {
    config::store_dir()
}

/// 网关凭证目录。
///
/// ★ 必须与网关侧 ai_gateway_router::config::default_trae_auth_dir()
///   逐字一致：两边分叉就会出现「界面里登录成功、网关看不到账号」。
///
/// 刻意**不是** store_dir().join("auths")：账号库就放在根目录，
/// 凭证再放同一个 auths 里会和将来的其它产品凭证混在一起
///（Qoder 用的是 ~/.wb-switch/qoder/auths，同一个约定）。
pub fn auth_dir() -> std::path::PathBuf {
    store_dir().join("trae").join("auths")
}

/// 确保凭证目录存在。
pub fn ensure_dirs() -> Result<(), String> {
    std::fs::create_dir_all(auth_dir()).map_err(|e| format!("创建 Trae 目录失败: {e}"))
}

/// 凭证文件名。
///
/// 与 zcode_account::cred_file_name 同款：uid 本身已带前缀，再拼一次会得到
/// trae-trae-xxx.json（Go 侧实测踩过这个坑）。
pub fn cred_file_name(uid: &str) -> String {
    let t = uid.trim();
    let base = if t.starts_with(CRED_PREFIX) {
        t.to_string()
    } else {
        format!("{CRED_PREFIX}{t}")
    };
    format!("{}.json", sanitize_uid(&base))
}

/// 把路径分隔符等替换掉，防止带 ../ 的 uid 写到目录外。
pub fn sanitize_uid(uid: &str) -> String {
    let mut out = String::with_capacity(uid.len());
    for c in uid.chars() {
        if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
            out.push('_');
        } else {
            out.push(c);
        }
    }
    while out.contains("..") {
        out = out.replace("..", "__");
    }
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

/// 从候选 uid 中筛出「应当导出到网关」的集合。
///
/// None = 不筛选（负载均衡 / 轮转，全部导出）；Some = 只保留集合内的
///（界面切到「手动」模式时勾选的账号）。
///
/// ★ 与 WorkBuddy 的 gateway::select_export_uids 逐字同款 —— 两套语义必须完全
///   一致，否则「切模式」在两个产品上表现不同。
fn select_export_uids(candidates: &[String], only: &Option<Vec<String>>) -> Vec<String> {
    match only {
        None => candidates.to_vec(),
        Some(allowed) => candidates
            .iter()
            .filter(|u| allowed.iter().any(|a| a == *u))
            .cloned()
            .collect(),
    }
}

/// 当前工作模式下应当导出的 uid 集合；None = 全部。
///
/// 复用 gateway 的模式判定（同一份配置，不另立一套）。
///
/// ★ 这里**不过滤 disabled**：禁用只表示「别把流量路由给它」，标记由
///   account.no_route 下传（见 build_credential_text），账号本身仍要导出 ——
///   否则网关的池里没有它，而池是唯一能遍历到它的地方，表现为「禁用后任务全停」。
fn active_uids() -> Option<Vec<String>> {
    if matches!(
        crate::modules::gateway::gateway_mode(),
        crate::modules::gateway::GatewayMode::Manual
    ) {
        Some(crate::modules::gateway::manual_uids())
    } else {
        None
    }
}

/// 把一条账号记录渲染成网关凭证。
///
/// 形态与 WorkBuddy 的网关凭证同款（auth + account 两段），
/// 因为网关侧的 products/trae/cred.rs::from_value 正是按两段式读的。
///
/// ★ 区域必须**显式**写出：网关的判定顺序是「显式 -> host/issuer -> userRegion」，
///   而实盘 JWT 里既没有 iss 也没有 userRegion，不写就是兜底 CN ——
///   国际账号会被发去国内主机，刷新必然失败。
///   取值复用 account_realm（它已经按 JWT issuer 的权威顺序判过），
///   写出 cn / sg 两个 id —— 与 trae_realm::Realm::as_str 逐字一致。
fn build_credential_text(acc: &Value) -> String {
    let user_id = acc
        .get("user_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let jwt = acc.get("jwt").and_then(Value::as_str).unwrap_or("").trim();
    let refresh = acc
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let realm = account_realm(acc).realm.as_str();

    let mut doc = json!({
        "schema": 1,
        "auth": {
            "token": jwt,
            "refreshToken": refresh,
            "realm": realm,
        },
        "account": {
            "uid": user_id,
            "user_id": user_id,
            "nickname": display_name(acc),
            "realm": realm,
        },
    });
    // 用户手动禁用 = 只不接流量，其余照跑（与 WorkBuddy 的 no_route 同一语义）。
    //
    // ★ 标记名用 no_route 而不是 disabled：网关自己也有一组 disabled
    //  （session 死 / 额度冻结，那种跑了也白跑），两者语义不同，混用会让
    //  「禁用即停任务」这个 bug 复现。
    if acc.get("disabled").and_then(Value::as_bool) == Some(true) {
        doc["account"]["no_route"] = json!(true);
    }
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

/// 把账号库导出成网关可读的凭证文件；返回 (写入条数, 变化清单)。
///
/// 由 gateway::start_gateway / gateway::sync_only 调用（与 WorkBuddy 同款时机）。
///
/// 刻意**不挂**在 run_auto_sync_loop 上：本函数建的目录是新建的，
/// 没有「升级前就存在的、内容已过期」的凭证需要收敛；周期性重写反而会
/// 覆盖网关自己刚刷新出来的 token（网关的 cred::save_refreshed 就写回同一个文件）。
pub fn export_accounts_to_gateway() -> Result<(usize, Vec<String>), String> {
    export_accounts_to_dir(&auth_dir(), &load_accounts(), active_uids())
}

/// export_accounts_to_gateway 的可注入目录版本（便于单测验证真实文件增删）。
pub fn export_accounts_to_dir(
    dir: &std::path::Path,
    accounts: &[Value],
    only: Option<Vec<String>>,
) -> Result<(usize, Vec<String>), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建凭证目录失败: {e}"))?;

    // user_id 空 = 无法进池（池以 uid 为键），跳过而不是造一个假 uid。
    let picked: Vec<(String, &Value)> = accounts
        .iter()
        .filter_map(|acc| {
            let uid = acc
                .get("user_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if uid.is_empty() || select_export_uids(&[uid.clone()], &only).is_empty() {
                None
            } else {
                Some((uid, acc))
            }
        })
        .collect();

    let mut written = 0usize;
    let mut changed: Vec<String> = Vec::new();
    for (uid, acc) in &picked {
        let path = dir.join(cred_file_name(uid));
        let text = build_credential_text(acc);
        // 内容完全一致就**不写盘**：写盘会让网关的凭证目录指纹变化，白跑一次热加载。
        if let Ok(existing) = std::fs::read_to_string(&path) {
            if existing.trim() == text.trim() {
                written += 1;
                continue;
            }
        }
        config::atomic_write(&path, &text).map_err(|e| format!("写入 Trae 凭证失败: {e}"))?;
        written += 1;
        changed.push(uid.clone());
    }

    // 清理：只删本目录里**形如 Trae 凭证**且已不在导出集合中的文件。
    //
    // ★ 判据必须与导出侧同源（都用 cred_file_name），否则切模式时
    //   会出现「刚导出就被删」或「删不干净」两种偏差。
    let live: Vec<String> = picked.iter().map(|(u, _)| cred_file_name(u)).collect();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with(CRED_PREFIX) || !name.ends_with(".json") {
                continue;
            }
            if live.contains(&name) {
                continue;
            }
            if std::fs::remove_file(e.path()).is_ok() {
                changed.push(format!("removed:{name}"));
            }
        }
    }

    Ok((written, changed))
}

/// 上次成功导出时的 Trae 账号库指纹。
static LAST_FINGERPRINT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 计算 Trae 账号库的内容指纹：uid + token 尾部 + refresh token 长度 + 禁用标记。
///
/// ★ 与 WorkBuddy 的 gateway::accounts_fingerprint 同款；token 只取尾部若干字符
///   参与指纹，不把完整凭证写进日志或内存字符串。
///
/// 为什么需要它：本模块导出的凭证**同时被网关写**（网关刷新 token 时会把
/// 新 token 写回同一个文件）。无条件周期性重写会立刻用账号库里的旧 JWT
/// 覆盖网关刚刷新出来的新 JWT —— 表现为「每次同步后第一个请求都要重新刷新一次」。
/// 只在本库真的变了时才导出，就把这个窗口缩到最小。
fn accounts_fingerprint() -> String {
    let accounts = load_accounts();
    let mut parts: Vec<String> = accounts
        .iter()
        .filter_map(|a| {
            let uid = a.get("user_id").and_then(Value::as_str)?;
            let jwt = a.get("jwt").and_then(Value::as_str).unwrap_or_default();
            let rt = a
                .get("refresh_token")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // token 只取尾部若干字符参与指纹（与 WorkBuddy 同款）。
            let jwt_tail: String = jwt.chars().rev().take(12).collect();
            let off = if a.get("disabled").and_then(Value::as_bool) == Some(true) {
                1
            } else {
                0
            };
            Some(format!("{uid}|{jwt_tail}|{}|{off}", rt.len()))
        })
        .collect();
    parts.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for p in &parts {
        for b in p.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
    }
    format!("{}-{:x}", parts.len(), hash)
}

/// 仅当 Trae 账号库有变化时才导出到网关凭证目录（供后台同步循环调用）。
///
/// 返回是否真的推送了。失败打日志并返回 false —— 与 WorkBuddy 的
/// gateway::sync_if_changed 同口径：这里是后台循环，不能把错误抛给用户。
pub fn export_if_changed() -> bool {
    let fp = accounts_fingerprint();
    {
        let guard = LAST_FINGERPRINT.lock().unwrap_or_else(|e| e.into_inner());
        if guard.as_deref() == Some(fp.as_str()) {
            return false;
        }
    }
    match export_accounts_to_gateway() {
        Ok(_) => {
            *LAST_FINGERPRINT.lock().unwrap_or_else(|e| e.into_inner()) = Some(fp);
            true
        }
        Err(e) => {
            eprintln!("[gateway] 自动同步 Trae 账号失败: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// 造一个 payload 可控的 JWT（签名段是占位，解析不校验签名）。
    fn make_jwt(payload: Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("{header}.{body}.sig")
    }

    #[test]
    fn 解析_jwt_的_uid_与过期时间() {
        let exp = chrono::Utc::now().timestamp() + 7200;
        let jwt = make_jwt(json!({"data": {"id": "2328112497170937"}, "exp": exp}));
        let info = parse_jwt(&jwt);
        assert_eq!(info.user_id.as_deref(), Some("2328112497170937"));
        assert_eq!(info.exp_timestamp, Some(exp));
        assert!(info.exp_hours.unwrap() > 1.9 && info.exp_hours.unwrap() < 2.1);
        assert_eq!(info.status(), "warn", "2 小时内过期应为 warn");
    }

    #[test]
    fn 解析_jwt_兼容三种前缀() {
        let jwt = make_jwt(json!({"data": {"id": "u1"}, "exp": 9999999999i64}));
        for candidate in [
            jwt.clone(),
            format!("Cloud-IDE-JWT {jwt}"),
            format!("Bearer {jwt}"),
            format!("  Cloud-IDE-JWT {jwt}  "),
        ] {
            assert_eq!(
                parse_jwt(&candidate).user_id.as_deref(),
                Some("u1"),
                "前缀形态 {candidate:.20}… 应能解析"
            );
        }
    }

    #[test]
    fn 解析_jwt_的_id_为数字时也能取到() {
        let jwt = make_jwt(json!({"data": {"id": 2328112497170937i64}}));
        assert_eq!(
            parse_jwt(&jwt).user_id.as_deref(),
            Some("2328112497170937")
        );
    }

    #[test]
    fn 解析_jwt_回退_auth_id_与_sub() {
        let a = make_jwt(json!({"auth_id": "from-auth-id"}));
        assert_eq!(parse_jwt(&a).user_id.as_deref(), Some("from-auth-id"));
        let b = make_jwt(json!({"sub": "from-sub"}));
        assert_eq!(parse_jwt(&b).user_id.as_deref(), Some("from-sub"));
    }

    #[test]
    fn 解析_jwt_对畸形输入不panic() {
        for bad in ["", "not-a-jwt", "a.b", "a.!!!.c", "...."] {
            let info = parse_jwt(bad);
            assert!(info.user_id.is_none() || !bad.is_empty());
        }
    }

    #[test]
    fn exp_为浮点或数字字符串时也能解析() {
        let f = make_jwt(json!({"data": {"id": "u"}, "exp": 9999999999.0}));
        assert_eq!(parse_jwt(&f).exp_timestamp, Some(9999999999));
        let s = make_jwt(json!({"data": {"id": "u"}, "exp": "9999999999"}));
        assert_eq!(parse_jwt(&s).exp_timestamp, Some(9999999999));
    }

    #[test]
    fn 状态判定阈值() {
        let mk = |h: Option<f64>| JwtInfo { exp_hours: h, ..Default::default() }.status();
        assert_eq!(mk(Some(100.0)), "ok");
        assert_eq!(mk(Some(25.0)), "ok");
        assert_eq!(mk(Some(24.0)), "warn");
        assert_eq!(mk(Some(0.5)), "warn");
        assert_eq!(mk(Some(0.0)), "expired");
        assert_eq!(mk(Some(-1.0)), "expired");
        assert_eq!(mk(None), "unknown");
    }

    #[test]
    fn 规范化_jwt_前缀幂等() {
        assert_eq!(normalize_jwt("abc"), "Cloud-IDE-JWT abc");
        assert_eq!(normalize_jwt("Cloud-IDE-JWT abc"), "Cloud-IDE-JWT abc");
        assert_eq!(normalize_jwt("Bearer abc"), "Bearer abc");
        assert_eq!(normalize_jwt("  abc  "), "Cloud-IDE-JWT abc");
    }

    /// ★ issuer 是区域判定的**权威线索**，必须能从 JWT 里取到。
    #[test]
    fn jwt_的_issuer_可被提取() {
        // 标准 `iss` 声明
        let j = make_jwt(json!({"data": {"id": "u"}, "iss": "https://growsg-normal.trae.ai"}));
        assert_eq!(
            parse_jwt(&j).issuer.as_deref(),
            Some("https://growsg-normal.trae.ai")
        );
        // 上游用过的变体 `issuer`
        let j2 = make_jwt(json!({"data": {"id": "u"}, "issuer": "https://api.trae.cn"}));
        assert_eq!(parse_jwt(&j2).issuer.as_deref(), Some("https://api.trae.cn"));
        // `iss` 优先于 `issuer`
        let j3 = make_jwt(json!({
            "data": {"id": "u"},
            "iss": "https://api.trae.cn",
            "issuer": "https://growsg-normal.trae.ai",
        }));
        assert_eq!(parse_jwt(&j3).issuer.as_deref(), Some("https://api.trae.cn"));
        // 缺失 / 空白 / 非字符串 → None（不得给出空串冒充「有 issuer」）
        assert_eq!(parse_jwt(&make_jwt(json!({"data": {"id": "u"}}))).issuer, None);
        assert_eq!(parse_jwt(&make_jwt(json!({"iss": "   "}))).issuer, None);
        assert_eq!(parse_jwt(&make_jwt(json!({"iss": 123}))).issuer, None);
        assert_eq!(parse_jwt("garbage").issuer, None);
    }

    /// ★★ 端到端：账号记录 → 区域判定，且 **issuer 压过 userRegion**。
    #[test]
    fn 账号区域判定以_issuer_为准() {
        use crate::modules::trae_realm::{Realm, RealmEvidence};

        // 国际账号：issuer 指向国际 auth host，但 userRegion 是 CN
        // （用户在中国用国际版）—— 必须以 issuer 为准，否则 token 刷新会
        // 被发去中国 auth host 而必然失败。
        let jwt = make_jwt(json!({
            "data": {"id": "u1"},
            "iss": "https://growsg-normal.trae.ai",
        }));
        let acc = json!({
            "user_id": "u1",
            "jwt": format!("Cloud-IDE-JWT {jwt}"),
            "user_region": "CN",
        });
        let g = account_realm(&acc);
        assert_eq!(g.realm, Realm::Sg, "issuer 指向国际版时不得被 userRegion 拉回");
        assert_eq!(g.evidence, RealmEvidence::HostOrIssuer);
        assert!(g.evidence.is_detected());

        // 国内账号
        let jwt_cn = make_jwt(json!({
            "data": {"id": "u2"},
            "iss": "https://api.trae.cn",
        }));
        let acc_cn = json!({"user_id": "u2", "jwt": format!("Cloud-IDE-JWT {jwt_cn}")});
        assert_eq!(account_realm(&acc_cn).realm, Realm::Cn);

        // 显式字段压过 issuer
        let acc_ex = json!({
            "user_id": "u1",
            "jwt": format!("Cloud-IDE-JWT {jwt}"),
            "_edition": "cn",
        });
        let g = account_realm(&acc_ex);
        assert_eq!(g.realm, Realm::Cn);
        assert_eq!(g.evidence, RealmEvidence::Explicit);

        // 什么线索都没有 → 兜底 CN 且**如实标注为未探测到**
        let acc_bare = json!({"user_id": "u3"});
        let g = account_realm(&acc_bare);
        assert_eq!(g.realm, Realm::Cn);
        assert_eq!(g.evidence, RealmEvidence::Default);
        assert!(!g.evidence.is_detected(), "兜底必须能被界面识别出来");

        // userRegion 为对象形态时也要能取到
        let acc_ur = json!({"user_id": "u4", "user_region": {"region": "SG"}});
        assert_eq!(account_realm(&acc_ur).realm, Realm::Sg);
    }

    /// 区域信息进入 `account_meta`，且**不泄露 JWT**。
    #[test]
    fn account_meta_带区域字段且不泄露_jwt() {
        let jwt = make_jwt(json!({
            "data": {"id": "u1"},
            "iss": "https://growsg-normal.trae.ai",
        }));
        let acc = json!({"user_id": "u1", "jwt": format!("Cloud-IDE-JWT {jwt}")});
        let meta = account_meta(&acc);
        assert_eq!(meta["realm"], "sg");
        assert_eq!(meta["realmEvidence"], "hostOrIssuer");
        assert_eq!(meta["realmDetected"], true);
        assert!(
            meta["realmLabel"].as_str().unwrap().contains("国际"),
            "标签应可读"
        );
        let text = serde_json::to_string(&meta).unwrap();
        assert!(!text.contains(&jwt), "JWT 原文不得出现在元数据里");
        assert!(!text.contains("growsg-normal.trae.ai"), "issuer 原文也不应外泄");
    }

    #[test]
    fn 脱敏只保留末几位() {
        assert_eq!(mask_tail("1234567890", 4), "******7890");
        assert_eq!(mask_tail("1234", 4), "****");
        assert_eq!(mask_tail("12", 4), "**");
        assert_eq!(mask_tail("", 4), "");
    }

    #[test]
    fn account_meta_不含_jwt_原文() {
        let exp = chrono::Utc::now().timestamp() + 100_000;
        let jwt = make_jwt(json!({"data": {"id": "u1"}, "exp": exp}));
        let acc = json!({
            "user_id": "u1",
            "name": "测试号",
            "jwt": format!("Cloud-IDE-JWT {jwt}"),
            "refresh_token": "rt-secret",
        });
        let meta = account_meta(&acc);
        let text = serde_json::to_string(&meta).unwrap();
        assert!(!text.contains("rt-secret"), "refresh_token 不得出现在元数据里");
        assert!(!text.contains(&jwt), "JWT 原文不得出现在元数据里");
        assert_eq!(meta["userId"], "u1");
        assert_eq!(meta["name"], "测试号");
        assert_eq!(meta["hasRefreshToken"], true);
        assert_eq!(meta["jwtStatus"], "ok");
    }

    #[test]
    fn 占位名识别() {
        // 捕获流程自动生成的两种占位名
        assert!(is_placeholder_name("auto_18839192"));
        assert!(is_placeholder_name("账号_18839192"));
        // 空名按占位处理（可被填上有信息量的名字）
        assert!(is_placeholder_name(""));
        assert!(is_placeholder_name("   "));
        // 用户取的名字一律不算占位
        assert!(!is_placeholder_name("主号"));
        assert!(!is_placeholder_name("Trae Work · Pro"));
        // 前缀像但不是占位（后面不是 uid 片段）→ 保守当作自定义名
        assert!(!is_placeholder_name("auto_"));
        assert!(!is_placeholder_name("账号_"));
        assert!(!is_placeholder_name("账号_主号"));
    }

    #[test]
    fn 只在占位名时才自动改名() {
        use crate::modules::config::test_isolation::Isolated;
        let _iso = Isolated::new("trae-rename");

        // 捕获自动建的账号（占位名）→ 可被填上套餐身份
        upsert_account("1883919207380040", None, "Cloud-IDE-JWT a.b.c", None).unwrap();
        assert!(
            is_placeholder_name(&display_name(&find_account("1883919207380040").unwrap())),
            "upsert 未给名字时应落成占位名"
        );
        assert!(set_name_if_placeholder("1883919207380040", "Trae Work · Pro"));
        assert_eq!(
            display_name(&find_account("1883919207380040").unwrap()),
            "Trae Work · Pro"
        );
        // 同名重复调用 → 无变化
        assert!(!set_name_if_placeholder("1883919207380040", "Trae Work · Pro"));

        // 用户手动改过名字 → **绝不**覆盖
        upsert_account("1994398417874030", Some("我的主号"), "Cloud-IDE-JWT a.b.c", None).unwrap();
        assert!(!set_name_if_placeholder("1994398417874030", "Trae · Free"));
        assert_eq!(
            display_name(&find_account("1994398417874030").unwrap()),
            "我的主号",
            "用户自定义名不得被自动改名覆盖"
        );

        // 账号不存在 / 空名字 → 安全返回 false
        assert!(!set_name_if_placeholder("no-such-uid", "x"));
        assert!(!set_name_if_placeholder("1883919207380040", "  "));
        assert_eq!(
            display_name(&find_account("1883919207380040").unwrap()),
            "Trae Work · Pro",
            "空名字不得把已有名字清掉"
        );
    }

    // -----------------------------------------------------------------------
    // 凭证导出
    // -----------------------------------------------------------------------

    /// 造一个独立的临时目录（导出写真实文件，必须隔离）。
    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "trae-account-export-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 一条最小可导出的账号记录。
    fn acc(uid: &str, jwt: &str) -> Value {
        json!({ "user_id": uid, "jwt": jwt, "refresh_token": format!("rt-{uid}") })
    }

    #[test]
    fn 凭证文件名不重复拼前缀且不留下路径分隔符() {
        assert_eq!(cred_file_name("trae-1001"), "trae-1001.json");
        assert_eq!(cred_file_name("1001"), "trae-1001.json", "未带前缀时补一次");
        assert_eq!(cred_file_name(" 1001 "), "trae-1001.json", "先 trim");
        // 带路径分隔符的 uid 绝不可逃出凭证目录：sanitize 之后既无 / 也无 ..
        let nasty = cred_file_name("../../evil");
        assert!(!nasty.contains('/'), "不得留下路径分隔符：{nasty}");
        assert!(!nasty.contains(".."), "不得留下上级目录：{nasty}");
        assert!(nasty.ends_with(".json"));
        assert_eq!(sanitize_uid(""), "unknown");
    }

    #[test]
    fn 导出会写出网关两段式凭证并显式写出区域() {
        let dir = tmp_dir("shape");
        let mut a = acc("1001", "Cloud-IDE-JWT a.b.c");
        a["realm"] = json!("sg");
        let (written, changed) = export_accounts_to_dir(&dir, &[a], None).unwrap();
        assert_eq!(written, 1);
        assert_eq!(changed, vec!["1001".to_string()]);

        let text = std::fs::read_to_string(dir.join("trae-1001.json")).unwrap();
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["auth"]["token"], "Cloud-IDE-JWT a.b.c");
        assert_eq!(doc["auth"]["refreshToken"], "rt-1001");
        // ★ 区域必须显式写出：实盘 JWT 里既没有 iss 也没有 userRegion，
        //   不写就会兜底到 CN，把国际账号发去国内主机。
        assert_eq!(doc["auth"]["realm"], "sg");
        assert_eq!(doc["account"]["uid"], "1001");
        assert_eq!(doc["account"]["user_id"], "1001");
        assert_eq!(doc["account"]["nickname"], "账号_1001");
        assert!(doc["account"].get("no_route").is_none(), "启用账号不写 no_route");
    }

    #[test]
    fn 用户禁用要照常导出_只把标识写成不接流量() {
        let dir = tmp_dir("disabled");
        let mut dead = acc("2002", "Cloud-IDE-JWT x.y.z");
        dead["disabled"] = json!(true);
        let (written, _) = export_accounts_to_dir(&dir, &[acc("1001", "j"), dead], None).unwrap();
        assert_eq!(
            written, 2,
            "被禁用的账号也必须导出 —— 否则网关的池里没有它，界面上的开关等于把账号删了"
        );

        let on: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("trae-1001.json")).unwrap())
                .unwrap();
        let off: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("trae-2002.json")).unwrap())
                .unwrap();
        assert!(on["account"].get("no_route").is_none());
        assert_eq!(
            off["account"]["no_route"],
            json!(true),
            "用户禁用 -> account.no_route（标记名与 WorkBuddy 一致）"
        );
    }

    #[test]
    fn 导出内容一致时不重复写盘() {
        let dir = tmp_dir("idem");
        let a = acc("1001", "Cloud-IDE-JWT a.b.c");
        let (n1, c1) = export_accounts_to_dir(&dir, &[a.clone()], None).unwrap();
        let (n2, c2) = export_accounts_to_dir(&dir, &[a], None).unwrap();
        assert_eq!((n1, n2), (1, 1));
        assert_eq!(c1, vec!["1001".to_string()]);
        assert!(c2.is_empty(), "内容未变就不得写盘：写盘会让网关白跑一次热加载");
    }

    #[test]
    fn 切到手动模式时会清掉不在名单里的凭证() {
        let dir = tmp_dir("manual");
        let (n, _) = export_accounts_to_dir(&dir, &[acc("1001", "j"), acc("2002", "j")], None).unwrap();
        assert_eq!(n, 2);

        let (n, changed) =
            export_accounts_to_dir(&dir, &[acc("1001", "j"), acc("2002", "j")], Some(vec!["1001".to_string()]))
                .unwrap();
        assert_eq!(n, 1, "只导出勾选的账号");
        assert!(dir.join("trae-1001.json").is_file());
        assert!(
            !dir.join("trae-2002.json").is_file(),
            "未勾选的账号必须从网关目录删除，否则网关仍会把流量路由过去"
        );
        assert!(
            changed.contains(&"removed:trae-2002.json".to_string()),
            "删除必须出现在变化清单里：网关要靠它决定是否重启：{changed:?}"
        );
    }

    #[test]
    fn 导出只清自己的凭证文件() {
        let dir = tmp_dir("foreign");
        std::fs::write(dir.join("workbuddy-1001.json"), "{}").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        let (_, changed) = export_accounts_to_dir(&dir, &[acc("1001", "j")], None).unwrap();
        assert!(
            dir.join("workbuddy-1001.json").is_file(),
            "导出 Trae 绝不能删别的产品的凭证"
        );
        assert!(dir.join("notes.txt").is_file());
        assert!(dir.join("trae-1001.json").is_file());
        assert_eq!(changed, vec!["1001".to_string()], "不应把外来文件记进变化清单");
    }

    #[test]
    fn 没有_user_id_的记录不进网关() {
        let dir = tmp_dir("nouser");
        let bare = json!({ "jwt": "j", "name": "没有 uid" });
        let (n, changed) = export_accounts_to_dir(&dir, &[bare, acc("1001", "j")], None).unwrap();
        assert_eq!(n, 1, "user_id 为空的账号无法进池，跳过而不是造一个假 uid");
        assert_eq!(changed, vec!["1001".to_string()]);
    }
}
