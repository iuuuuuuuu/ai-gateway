//! 官方认证文件 `workbuddy-desktop.info` 的路径与读写（四段 JSON）。
//!
//! 对照 server.py `auth_file_path` / `workbuddy_app_path` / `read_auth_file` /
//! `import_from_auth_file`。切换写入（build_account_obj / build_auth_obj /
//! write_account_to_auth_file）在阶段 2 随 switch.rs 落地。

use serde_json::{json, Map, Value};
use std::path::PathBuf;

use crate::modules::account::get_str;
use crate::modules::config::{atomic_write, backup_dir, now_ms, utc_iso, Region};

/// WorkBuddy 官方认证文件路径（与 cockpit 一致）。
/// 认证文件所在目录（国服与国际版共用同一目录）。
pub fn auth_dir() -> PathBuf {
    let home = crate::modules::config::home_dir();
    #[cfg(target_os = "macos")]
    return home.join("Library/Application Support/CodeBuddyExtension/Data/Public/auth");
    #[cfg(target_os = "windows")]
    return home.join("AppData/Local/CodeBuddyExtension/Data/Public/auth");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    return home.join(".local/share/CodeBuddyExtension/Data/Public/auth");
}

/// 指定区域的认证文件名。
///
/// 国服与国际版由客户端写入**不同文件**（同一目录下）：
///   workbuddy-desktop.info      国服
///   workbuddy-desktop-ai.info   国际版
pub fn auth_file_name_for(region: crate::modules::config::Region) -> &'static str {
    use crate::modules::config::Region;
    match region {
        Region::Cn => "workbuddy-desktop.info",
        Region::Intl => "workbuddy-desktop-ai.info",
    }
}

/// 指定区域的认证文件路径。
pub fn auth_file_path_for(region: crate::modules::config::Region) -> PathBuf {
    auth_dir().join(auth_file_name_for(region))
}

/// 账号所属区域的认证文件路径。
///
/// 切换必须按**账号自身的区域**选文件：国际版账号写进国服的
/// `workbuddy-desktop.info` 会让客户端带着错配的身份请求，服务端表现为
/// 「账号访问受限」。区域由账号 domain 推导（`.ai` → 国际版）。
pub fn auth_file_path_of(acc: &Value) -> PathBuf {
    auth_file_path_for(crate::modules::config::Region::of(acc))
}

/// 默认（国服）认证文件路径。保留原签名以避免影响既有调用点。
pub fn auth_file_path() -> PathBuf {
    auth_file_path_for(crate::modules::config::Region::Cn)
}

/// WorkBuddy 应用路径（指定区域）。
///
/// 探测顺序：运行进程 Path → 缓存 → 注册表 → 环境变量/盘符扫描。
/// 都找不到时返回 LOCALAPPDATA 默认路径，供启动失败文案写出尝试路径。
pub fn workbuddy_app_path_for(region: crate::modules::config::Region) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(exe) = crate::modules::process::windows_workbuddy_exe_path(region) {
            return exe;
        }
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let (dir, exe_name) = match region {
            crate::modules::config::Region::Cn => ("WorkBuddy", "WorkBuddy.exe"),
            crate::modules::config::Region::Intl => ("WorkBuddyAI", "WorkBuddyAI.exe"),
        };
        return std::path::Path::new(&local)
            .join("Programs")
            .join(dir)
            .join(exe_name);
    }
    #[cfg(not(target_os = "windows"))]
    {
        // macOS/Linux 只有单一客户端，区域参数仅用于保持调用方签名一致。
        let _ = region;
        crate::modules::process::macos_workbuddy_app_path()
    }
}

/// 状态展示用：任一区域已解析到的客户端路径，否则国服默认路径。
pub fn workbuddy_app_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        use crate::modules::config::Region;
        for region in [Region::Cn, Region::Intl] {
            if let Some(exe) = crate::modules::process::windows_workbuddy_exe_path(region) {
                return exe;
            }
        }
        workbuddy_app_path_for(Region::Cn)
    }
    #[cfg(not(target_os = "windows"))]
    {
        crate::modules::process::macos_workbuddy_app_path()
    }
}

/// 读取认证文件 JSON；不存在或解析失败返回 None。
/// 读取指定区域的认证文件；不存在或解析失败返回 None。
pub fn read_auth_file_for(region: crate::modules::config::Region) -> Option<Value> {
    let path = auth_file_path_for(region);
    if !path.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn read_auth_file() -> Option<Value> {
    let path = auth_file_path();
    if !path.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 切换前备份当前认证文件，返回备份路径。对照 server.py `backup_auth_file`。
pub fn backup_auth_file() -> Option<PathBuf> {
    backup_auth_file_at(&auth_file_path())
}

/// 备份指定区域的认证文件（国服 / 国际版各一份，互不覆盖）。
pub fn backup_auth_file_for(region: crate::modules::config::Region) -> Option<PathBuf> {
    backup_auth_file_at(&auth_file_path_for(region))
}

fn backup_auth_file_at(path: &std::path::Path) -> Option<PathBuf> {
    if !path.exists() {
        return None;
    }
    let dir = backup_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let ts = utc_iso();
    // 备份名带上区域，避免国服与国际版备份互相覆盖。
    let stem = path.file_stem()?.to_string_lossy().to_string();
    let dest = dir.join(format!("{stem}.{ts}.info"));
    std::fs::copy(path, &dest).ok()?;
    Some(dest)
}

/// 从账号库记录构造官方 account 字段。对照 server.py `build_account_obj`。
pub fn build_account_obj(acc: &Value) -> Value {
    let mut obj: Map<String, Value> = match acc.get("profile_raw") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    obj.insert(
        "uid".to_string(),
        acc.get("uid").cloned().unwrap_or_else(|| json!("")),
    );
    obj.insert(
        "nickname".to_string(),
        acc.get("nickname").cloned().unwrap_or_else(|| json!("")),
    );
    setdefault(&mut obj, "type", json!("personal"));
    setdefault(&mut obj, "accountType", json!(""));
    setdefault(&mut obj, "idp", json!(""));
    setdefault(&mut obj, "oneidAccountId", json!(""));
    setdefault(&mut obj, "areaInfoComplete", json!(false));
    setdefault(&mut obj, "isCurrentOneIdEnterprise", json!(false));
    setdefault(&mut obj, "isCurrentOneIdPersonal", json!(false));
    setdefault(&mut obj, "isFirstLogin", json!(false));
    setdefault(&mut obj, "isCreator", json!(false));
    setdefault(&mut obj, "isAdmin", json!(false));
    setdefault(&mut obj, "uin", json!(""));
    setdefault(&mut obj, "phoneNumber", json!(""));
    setdefault(&mut obj, "lastLogin", json!(true));
    setdefault(&mut obj, "pluginEnabled", json!(true));
    setdefault(
        &mut obj,
        "deployStatus",
        json!({"statusCode": 0, "statusMsg": "", "detailMsg": ""}),
    );
    setdefault(
        &mut obj,
        "sso",
        json!({"domain": "", "domainModifiedTimes": 0}),
    );
    Value::Object(obj)
}

/// 从账号库记录构造官方 auth 字段。对照 server.py `build_auth_obj`。
pub fn build_auth_obj(acc: &Value) -> Value {
    let mut obj: Map<String, Value> = Map::new();
    let raw = acc.get("auth_raw");
    if let Some(Value::Object(m)) = raw {
        let inner = match m.get("auth") {
            Some(Value::Object(im)) => im.clone(),
            _ => m.clone(),
        };
        obj.extend(inner);
    }
    let token_type = acc
        .get("token_type")
        .and_then(|v| v.as_str())
        .unwrap_or("Bearer")
        .to_string();
    let expires_at = acc.get("expiresAt").and_then(|v| v.as_i64());
    let now = now_ms();

    obj.insert(
        "accessToken".to_string(),
        get_str(acc, "access_token").unwrap_or_default().into(),
    );
    obj.insert(
        "refreshToken".to_string(),
        get_str(acc, "refresh_token").unwrap_or_default().into(),
    );
    obj.insert("tokenType".to_string(), token_type.into());
    obj.insert(
        "domain".to_string(),
        get_str(acc, "domain").unwrap_or_default().into(),
    );
    obj.insert("lastRefreshTime".to_string(), json!(now));
    setdefault(
        &mut obj,
        "scope",
        json!("openid profile offline_access email"),
    );

    if let Some(expires_at) = expires_at {
        obj.insert("expiresAt".to_string(), json!(expires_at));
        obj.insert(
            "expiresIn".to_string(),
            json!(((expires_at - now) / 1000).max(0)),
        );
        let refresh_exp = raw
            .and_then(|r| r.get("refreshExpiresAt"))
            .and_then(|v| v.as_i64())
            .unwrap_or(expires_at);
        if !obj.contains_key("refreshExpiresAt") {
            obj.insert("refreshExpiresAt".to_string(), json!(refresh_exp));
        }
        obj.insert(
            "refreshExpiresIn".to_string(),
            json!(((refresh_exp - now) / 1000).max(0)),
        );
    } else {
        setdefault(&mut obj, "expiresIn", json!(0));
        setdefault(&mut obj, "refreshExpiresIn", json!(0));
    }
    setdefault(&mut obj, "notBeforePolicy", json!(0));
    setdefault(&mut obj, "sessionState", json!(""));
    Value::Object(obj)
}

/// 把账号写入官方认证文件（原子写 + 写后校验）。对照 server.py `write_account_to_auth_file`。
///
/// **按账号区域写入对应的认证文件**：国际版写 `workbuddy-desktop-ai.info`，
/// 国服写 `workbuddy-desktop.info`。两者是客户端读取的**不同文件**，写错会
/// 导致目标区域登录态根本没更新（并让客户端带着另一区域的 token 请求）。
pub fn write_account_to_auth_file(acc: &Value) -> Result<(), String> {
    write_account_to_auth_file_at(&auth_file_path_of(acc), acc)
}

/// 把账号写入指定路径的认证文件（供区域路由与测试复用）。
pub fn write_account_to_auth_file_at(path: &std::path::Path, acc: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let existing = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| json!({}));
    eprintln!(
        "[auth] write_account: existing is_object={} allAccounts_len={}",
        existing.is_object(),
        existing
            .get("allAccounts")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
    );
    let all_accounts = existing
        .get("allAccounts")
        .cloned()
        .or_else(|| existing.get("accounts").cloned())
        .filter(|v| v.is_array())
        .unwrap_or_else(|| json!([]));
    let account_obj = build_account_obj(acc);
    let auth_obj = build_auth_obj(acc);

    // 把目标账号并入 allAccounts（去重：按 uid 或 id）
    let target_uid = get_str(acc, "uid").unwrap_or_default();
    let mut all: Vec<Value> = all_accounts.as_array().cloned().unwrap_or_default();
    all.retain(|a| {
        let primary = a
            .get("uid")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                a.get("id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or("");
        primary != target_uid
    });
    all.push(account_obj.clone());
    eprintln!("[auth] write_account: merged allAccounts len={}", all.len());

    let session = json!({
        "account": &account_obj,
        "auth": &auth_obj,
        "accounts": &all,
        "allAccounts": &all,
    });
    let content = serde_json::to_string_pretty(&session).map_err(|e| e.to_string())?;
    if let Err(e) = atomic_write(path, &content) {
        eprintln!("[auth] atomic_write FAILED: {e}");
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            return Err(
                "无权限写入认证文件：请打开 系统设置→隐私与安全性→App 管理，允许本 App 控制 WorkBuddy 的数据（或为其开启『完全磁盘访问』后重试）"
                    .to_string(),
            );
        }
        return Err(e.to_string());
    }

    // 写后校验
    let written: Value =
        serde_json::from_str(&std::fs::read_to_string(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let written_token = written
        .get("auth")
        .and_then(|a| a.get("accessToken"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let expect_token = get_str(acc, "access_token").unwrap_or_default();
    if written_token != expect_token {
        return Err("认证文件写后校验失败，未写入目标账号".to_string());
    }
    Ok(())
}

fn setdefault(map: &mut Map<String, Value>, key: &str, value: Value) {
    if !map.contains_key(key) {
        map.insert(key.to_string(), value);
    }
}

/// 从当前 WorkBuddy 登录态导入账号。对照 server.py `import_from_auth_file`。
/// 从指定区域的认证文件导入账号。
pub fn import_from_auth_file_for(region: crate::modules::config::Region) -> Option<Value> {
    imported_account_from_root(read_auth_file_for(region)?)
}

pub fn import_from_auth_file() -> Option<Value> {
    imported_account_from_root(read_auth_file()?)
}

fn imported_account_from_root(root: Value) -> Option<Value> {
    let account_obj = root
        .get("account")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let auth_obj = root
        .get("auth")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));

    let uid = get_str(&root, "uid").or_else(|| get_str(&account_obj, "uid"));
    let uid = uid.or_else(|| get_str(&account_obj, "id"));
    let nickname = get_str(&root, "nickname")
        .or_else(|| get_str(&root, "name"))
        .or_else(|| get_str(&account_obj, "nickname"))
        .or_else(|| get_str(&account_obj, "label"));
    let email = get_str(&root, "email")
        .or_else(|| get_str(&account_obj, "email"))
        .or_else(|| get_str(&auth_obj, "email"));
    let access_token = get_str(&auth_obj, "accessToken")
        .or_else(|| get_str(&auth_obj, "access_token"))
        .or_else(|| get_str(&root, "accessToken"))
        .or_else(|| get_str(&root, "access_token"));
    let refresh_token = get_str(&auth_obj, "refreshToken")
        .or_else(|| get_str(&auth_obj, "refresh_token"))
        .or_else(|| get_str(&root, "refreshToken"))
        .or_else(|| get_str(&root, "refresh_token"));
    let token_type = get_str(&auth_obj, "tokenType")
        .or_else(|| get_str(&auth_obj, "token_type"))
        .unwrap_or_else(|| "Bearer".to_string());
    let domain = get_str(&root, "domain").or_else(|| get_str(&auth_obj, "domain"));
    let expires_at = parse_ts(root.get("expiresAt").or_else(|| auth_obj.get("expiresAt")));
    let refresh_expires_at = parse_ts(
        root.get("refreshExpiresAt")
            .or_else(|| auth_obj.get("refreshExpiresAt")),
    );

    if access_token.is_none() {
        return None;
    }

    Some(json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "uid": uid,
        "nickname": nickname,
        "email": email,
        "enterpriseName": get_str(&root, "enterpriseName")
            .or_else(|| get_str(&root, "enterprise_name"))
            .or_else(|| get_str(&account_obj, "enterpriseName"))
            .or_else(|| get_str(&account_obj, "enterprise_name")),
        "enterpriseId": get_str(&root, "enterpriseId")
            .or_else(|| get_str(&root, "enterprise_id"))
            .or_else(|| get_str(&account_obj, "enterpriseId"))
            .or_else(|| get_str(&account_obj, "enterprise_id")),
        "access_token": access_token,
        "refresh_token": refresh_token,
        "token_type": token_type,
        "domain": domain,
        "expiresAt": expires_at,
        "refreshExpiresAt": refresh_expires_at,
        "auth_raw": root,
        "profile_raw": account_obj,
        "createdAt": now_ms(),
    }))
}

/// 字符串时间戳转 i64（数字原样保留，不做秒/毫秒换算）。
/// 对照 server.py `import_from_auth_file` 的 str→int 逻辑。
fn parse_ts(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok().map(|f| f as i64),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 本机历史登录态发现（当前认证文件 + 客户端快照 + 本工具备份）
// ---------------------------------------------------------------------------
//
// 背景：`read_auth_file_for` 只读两个固定文件名，因此「从本机导入」每区域
// 最多只能拿到**当前**登录的 1 个账号。但客户端每次登录/切换都会把当时的
// 登录态另存一份快照（`workbuddy-desktop.<UTC 时间>.<pid>.<uuid>.info`），
// 本工具每次切换前也会备份（`~/.ai-gateway/backups/<stem>.<UTC 时间>.info`）。
// 这些文件里躺着本机历史上登录过的账号，过去完全没有入口能读到它们。
//
// 本节提供扫描与去重：同一 (区域, uid) 可能有多份文件，只保留**凭证最新**
// 的一份（后来的登录会让服务端轮换 refresh token，旧快照里的 refresh token
// 已失效，混用会互相顶掉）。

/// 认证文件的来源类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFileSource {
    /// 客户端**当前**登录态（固定文件名，无时间戳后缀）。
    Current,
    /// 历史登录快照（客户端留存的带时间戳 `.info`）。
    Snapshot,
    /// 本工具切换前的备份（`~/.ai-gateway/backups/`）。
    Backup,
}

impl AuthFileSource {
    pub fn key(self) -> &'static str {
        match self {
            AuthFileSource::Current => "current",
            AuthFileSource::Snapshot => "snapshot",
            AuthFileSource::Backup => "backup",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AuthFileSource::Current => "当前登录",
            AuthFileSource::Snapshot => "历史快照",
            AuthFileSource::Backup => "切换备份",
        }
    }
}

/// 凭证可用性分级。排序即优先级，用于在同一账号的多份文件间选优。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CredentialFreshness {
    /// access 与 refresh 都已过期：导入也没有实际价值。
    Expired,
    /// refresh 已过期，但 access token 仍在有效期内。
    AccessOnly,
    /// refresh token 仍在有效期内（可继续保活，最有价值）。
    Refreshable,
}

impl CredentialFreshness {
    pub fn key(self) -> &'static str {
        match self {
            CredentialFreshness::Expired => "expired",
            CredentialFreshness::AccessOnly => "access_only",
            CredentialFreshness::Refreshable => "refreshable",
        }
    }

    /// 是否值得导入（未完全过期）。
    pub fn usable(self) -> bool {
        self != CredentialFreshness::Expired
    }
}

/// 本机发现的一个候选账号（一对一的认证文件来源）。
#[derive(Debug, Clone)]
pub struct LocalCandidate {
    /// 账号记录，与 `save_collected_account` 的入参同构。
    pub account: Value,
    /// 来源区域（以**文件名**为准，不依赖文件内容里的 domain）。
    pub region: Region,
    /// 来源类型。
    pub source: AuthFileSource,
    /// 来源文件的绝对路径，作为跨扫描稳定的选择键。
    pub path: PathBuf,
    /// 来源文件名（展示用）。
    pub file_name: String,
    /// 文件最后修改时间（毫秒），用于在同类凭证间判断新旧。
    pub modified_at: i64,
    /// 凭证可用性。
    pub freshness: CredentialFreshness,
    /// 同一 (区域, uid) 在本机共有多少份文件（含自身）。
    pub duplicate_count: usize,
}

/// 账号凭证的到期时间（优先 refresh，其次 access），用于比较新旧。
pub fn credential_expiry(acc: &Value) -> Option<i64> {
    parse_ts(acc.get("refreshExpiresAt")).or_else(|| parse_ts(acc.get("expiresAt")))
}

/// 判定凭证可用性。缺少到期时间的 refresh token 视为可用——服务端才是最终
/// 裁判，不应仅因文件没写到期时间就丢弃一个可能仍有效的账号。
pub fn credential_freshness(acc: &Value, now: i64) -> CredentialFreshness {
    let rt = get_str(acc, "refresh_token");
    let rt_exp = parse_ts(acc.get("refreshExpiresAt"));
    if rt.is_some() && rt_exp.map(|e| e > now).unwrap_or(true) {
        return CredentialFreshness::Refreshable;
    }
    if parse_ts(acc.get("expiresAt")).map(|e| e > now).unwrap_or(false) {
        return CredentialFreshness::AccessOnly;
    }
    CredentialFreshness::Expired
}

/// 从认证文件名推断区域与来源；无法识别（非本工具关心的文件）返回 None。
///
/// 已知命名（同一目录，国服与国际版靠 `-ai` 后缀区分）：
///   `workbuddy-desktop.info`            国服当前登录
///   `workbuddy-desktop-ai.info`         国际版当前登录
///   `workbuddy-desktop.<ts>.<pid>.<uuid>.info`     客户端快照（国服）
///   `workbuddy-desktop-ai.<ts>.<pid>.<uuid>.info`  客户端快照（国际版）
///
/// 其余文件（如 `Tencent-Cloud.coding-copilot.*.info`）不属于本工具管辖，
/// 一律忽略。
pub fn parse_auth_file_name(name: &str) -> Option<(Region, AuthFileSource)> {
    let name = name.trim();
    if !name.to_ascii_lowercase().ends_with(".info") {
        return None;
    }
    // 先判当前登录的固定文件名，再判带后缀的快照/备份。
    if name == auth_file_name_for(Region::Cn) {
        return Some((Region::Cn, AuthFileSource::Current));
    }
    if name == auth_file_name_for(Region::Intl) {
        return Some((Region::Intl, AuthFileSource::Current));
    }
    // 注意先判 `-ai.`：`workbuddy-desktop-ai.` 不以 `workbuddy-desktop.` 开头，
    // 但把顺序写死可避免将来加后缀时踩坑。
    if name.starts_with("workbuddy-desktop-ai.") {
        return Some((Region::Intl, AuthFileSource::Snapshot));
    }
    if name.starts_with("workbuddy-desktop.") {
        return Some((Region::Cn, AuthFileSource::Snapshot));
    }
    None
}

/// 读取单个认证文件为候选账号；文件缺失/损坏/缺 token 返回 None。
fn read_candidate_at(
    path: &std::path::Path,
    region: Region,
    source: AuthFileSource,
) -> Option<LocalCandidate> {
    let text = std::fs::read_to_string(path).ok()?;
    let root: Value = serde_json::from_str(&text).ok()?;
    let mut account = imported_account_from_root(root)?;

    // 区域以**文件名**为准：备份文件与手工整理过的快照可能缺 domain，
    // 而 Region::from_domain 缺省按国服处理，会把国际版账号记错区域。
    let missing_domain = get_str(&account, "domain").is_none();
    if missing_domain {
        account["domain"] = json!(region.auth_domain());
    }

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let modified_at = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or_else(now_ms);
    let freshness = credential_freshness(&account, now_ms());

    Some(LocalCandidate {
        account,
        region,
        source,
        path: path.to_path_buf(),
        file_name,
        modified_at,
        freshness,
        duplicate_count: 1,
    })
}

/// 扫描一个目录下的全部候选认证文件。
fn scan_dir(dir: &std::path::Path, source: AuthFileSource) -> Vec<LocalCandidate> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((region, file_source)) = parse_auth_file_name(&name) else {
            continue;
        };
        // 备份目录里的文件统一标为 Backup，其余按文件名判定。
        let file_source = match source {
            AuthFileSource::Backup => AuthFileSource::Backup,
            _ => file_source,
        };
        if let Some(candidate) = read_candidate_at(&path, region, file_source) {
            found.push(candidate);
        }
    }
    found
}

/// 候选账号的身份键：区域 + uid（缺 uid 时退回真实邮箱，再退回文件路径）。
fn candidate_identity(candidate: &LocalCandidate) -> String {
    let region = candidate.region.key();
    if let Some(uid) = get_str(&candidate.account, "uid") {
        return format!("{region}\u{1}{uid}");
    }
    if let Some(email) = get_str(&candidate.account, "email") {
        return format!("{region}\u{1}email:{email}");
    }
    // 既无 uid 又无邮箱：无法判定为同一账号，按文件独立保留。
    format!("{region}\u{1}file:{}", candidate.path.to_string_lossy())
}

/// 候选之间的优先级排序：先比凭证可用性，再比到期时间，最后比文件新旧。
fn candidate_rank(candidate: &LocalCandidate) -> (CredentialFreshness, i64, i64) {
    (
        candidate.freshness,
        credential_expiry(&candidate.account).unwrap_or(0),
        candidate.modified_at,
    )
}

/// 扫描全部已知位置，返回去重后的候选账号。
///
/// 去重规则：同一 (区域, uid) 只保留优先级最高的一份——先用凭证可用性
/// （可刷新 > 仅 access 有效 > 已过期）排序，同级再比到期时间与文件修改时间。
/// 这样「最近一次登录/刷新写下的那份」总会胜出，而不至于让一份 refresh token
/// 已被轮换掉的旧快照顶掉有效凭证。
///
/// 返回顺序固定（按优先级降序，同级按路径升序），便于界面展示与按键选择。
pub fn discover_local_accounts() -> Vec<LocalCandidate> {
    discover_local_accounts_in(&auth_dir(), &backup_dir())
}

/// 扫描结果：去重后的候选 + 原始文件数（供界面说明「N 份文件 → M 个账号」）。
#[derive(Debug, Clone)]
pub struct LocalDiscovery {
    /// 去重后的候选账号。
    pub candidates: Vec<LocalCandidate>,
    /// 识别出的认证文件总数（含被去重掉的旧快照）。
    pub files_scanned: usize,
    /// 认证文件目录。
    pub auth_dir: PathBuf,
    /// 本工具备份目录。
    pub backup_dir: PathBuf,
}

/// `discover_local_accounts` 的可注入目录版本（便于单测不触碰真实用户目录）。
pub fn discover_local_accounts_in(
    auth_dir: &std::path::Path,
    backup_dir: &std::path::Path,
) -> Vec<LocalCandidate> {
    discover_local_in(auth_dir, backup_dir).candidates
}

/// 带统计信息的完整扫描。
pub fn discover_local_in(auth_dir: &std::path::Path, backup_dir: &std::path::Path) -> LocalDiscovery {
    let mut all = scan_dir(auth_dir, AuthFileSource::Snapshot);
    all.extend(scan_dir(backup_dir, AuthFileSource::Backup));
    let files_scanned = all.len();
    LocalDiscovery {
        candidates: dedupe_candidates(all),
        files_scanned,
        auth_dir: auth_dir.to_path_buf(),
        backup_dir: backup_dir.to_path_buf(),
    }
}

/// 扫描本机全部已知位置（当前目录 + 备份目录），带文件数统计。
pub fn discover_local() -> LocalDiscovery {
    discover_local_in(&auth_dir(), &backup_dir())
}

/// 纯函数：把候选列表按「区域 + 身份」去重并排序。见 `discover_local_accounts`。
fn dedupe_candidates(all: Vec<LocalCandidate>) -> Vec<LocalCandidate> {
    // 统计每个身份共有多少份文件（用于界面提示「另有 N 份旧快照」）。
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for candidate in &all {
        *counts.entry(candidate_identity(candidate)).or_insert(0) += 1;
    }

    let mut best: std::collections::HashMap<String, LocalCandidate> =
        std::collections::HashMap::new();
    for candidate in all {
        let key = candidate_identity(&candidate);
        match best.get(&key) {
            Some(existing) if candidate_rank(existing) >= candidate_rank(&candidate) => continue,
            _ => {}
        }
        best.insert(key, candidate);
    }

    let mut result: Vec<LocalCandidate> = best.into_values().collect();
    for candidate in result.iter_mut() {
        candidate.duplicate_count = *counts
            .get(&candidate_identity(candidate))
            .unwrap_or(&1_usize);
    }

    result.sort_by(|a, b| {
        candidate_rank(b)
            .cmp(&candidate_rank(a))
            .then_with(|| a.region.key().cmp(b.region.key()))
            .then_with(|| {
                a.path
                    .to_string_lossy()
                    .to_lowercase()
                    .cmp(&b.path.to_string_lossy().to_lowercase())
            })
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn auth_file_path_is_expected_location() {
        let p = auth_file_path();
        let s = p.to_string_lossy();
        assert!(
            s.contains("CodeBuddyExtension"),
            "路径应包含 CodeBuddyExtension: {s}"
        );
        assert!(
            s.ends_with("workbuddy-desktop.info"),
            "文件名应为 workbuddy-desktop.info: {s}"
        );
    }

    #[test]
    fn import_from_auth_file_extracts_fields() {
        let root = json!({
            "account": {"uid": "u-1", "nickname": "小明", "email": "a@b.c"},
            "auth": {
                "accessToken": "AT-1",
                "refreshToken": "RT-1",
                "tokenType": "Bearer",
                "domain": "www.codebuddy.cn",
                "expiresAt": "1791912333558",
            },
            "domain": "www.codebuddy.cn",
        });
        // import_from_auth_file 从真实认证文件读取，此处直接测 parse_ts 与字段提取逻辑
        assert_eq!(parse_ts(root["auth"].get("expiresAt")), Some(1791912333558));
        assert_eq!(parse_ts(root["auth"].get("refreshToken")), None);
        assert_eq!(parse_ts(Some(&json!("1786728333"))), Some(1786728333));
    }

    #[test]
    fn auth_file_path_of_routes_by_account_region() {
        use crate::modules::config::Region;

        let cn = json!({"uid": "u-cn", "domain": "www.codebuddy.cn"});
        let intl = json!({"uid": "u-ai", "domain": "www.workbuddy.ai"});
        // 缺 domain 的历史账号按国服处理
        let legacy = json!({"uid": "u-legacy"});

        assert_eq!(
            auth_file_path_of(&cn).file_name().unwrap(),
            "workbuddy-desktop.info"
        );
        assert_eq!(
            auth_file_path_of(&intl).file_name().unwrap(),
            "workbuddy-desktop-ai.info"
        );
        assert_eq!(
            auth_file_path_of(&legacy).file_name().unwrap(),
            "workbuddy-desktop.info"
        );
        // 回归保护：国际版绝不能被路由到国服文件
        assert_ne!(
            auth_file_path_of(&intl),
            auth_file_path_for(Region::Cn),
            "国际版账号必须写入 workbuddy-desktop-ai.info"
        );
    }

    /// 回归保护：国际版账号的写入必须落到 ai 文件，且不污染国服文件。
    #[test]
    fn write_routes_intl_account_to_ai_file_only() {
        let dir = std::env::temp_dir().join(format!(
            "ai-gateway-authfile-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let cn_path = dir.join("workbuddy-desktop.info");
        let ai_path = dir.join("workbuddy-desktop-ai.info");

        std::fs::write(&cn_path, json!({"auth": {"accessToken": "CN-KEEP"}}).to_string())
            .expect("seed cn file");

        let intl = json!({
            "id": "a-intl",
            "uid": "u-ai",
            "domain": "www.workbuddy.ai",
            "access_token": "AI-TOKEN",
            "refresh_token": "AI-REFRESH",
        });
        write_account_to_auth_file_at(&ai_path, &intl).expect("write intl");

        // 国际版 token 落在 ai 文件
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&ai_path).unwrap()).unwrap();
        assert_eq!(written["auth"]["accessToken"], "AI-TOKEN");

        // 国服文件保持原样（未被国际版写入污染）
        let cn: Value = serde_json::from_str(&std::fs::read_to_string(&cn_path).unwrap()).unwrap();
        assert_eq!(cn["auth"]["accessToken"], "CN-KEEP");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn import_without_email_does_not_synthesize_one() {
        let account = imported_account_from_root(json!({
            "account": {"uid": "u-1", "nickname": "同名用户"},
            "auth": {"accessToken": "test-token"}
        }))
        .expect("auth payload should import");

        assert_eq!(account["uid"], "u-1");
        assert_eq!(account["nickname"], "同名用户");
        assert!(account["email"].is_null());
    }

    // ---- 本机历史登录态发现 ----

    #[test]
    fn parse_auth_file_name_classifies_known_names_only() {
        use super::AuthFileSource as S;
        assert_eq!(
            parse_auth_file_name("workbuddy-desktop.info"),
            Some((Region::Cn, S::Current))
        );
        assert_eq!(
            parse_auth_file_name("workbuddy-desktop-ai.info"),
            Some((Region::Intl, S::Current))
        );
        assert_eq!(
            parse_auth_file_name(
                "workbuddy-desktop.2026-08-30T08-40-52-204Z.7476.a33c8c48-2a90-4dae-b0ec-3557a35e489e.info"
            ),
            Some((Region::Cn, S::Snapshot))
        );
        assert_eq!(
            parse_auth_file_name(
                "workbuddy-desktop-ai.2026-09-13T14-58-02-411Z.34260.3ebf98e6.info"
            ),
            Some((Region::Intl, S::Snapshot))
        );
        // 国际版快照绝不能落到国服分支（`-ai.` 前缀判定必须先命中）
        assert_ne!(
            parse_auth_file_name("workbuddy-desktop-ai.x.info").map(|(r, _)| r),
            Some(Region::Cn)
        );
        // 非本工具管辖的文件一律忽略
        assert_eq!(
            parse_auth_file_name("Tencent-Cloud.coding-copilot.2026-09-09T05-19-16-805Z.info"),
            None
        );
        assert_eq!(parse_auth_file_name("accounts.json"), None);
    }

    /// 造一个带完整凭证字段的认证文件内容。
    fn auth_payload(uid: &str, nick: &str, access: &str, refresh: &str, exp: i64, rt_exp: i64) -> String {
        json!({
            "account": {"uid": uid, "nickname": nick},
            "auth": {
                "accessToken": access,
                "refreshToken": refresh,
                "tokenType": "Bearer",
                "expiresAt": exp,
                "refreshExpiresAt": rt_exp,
                "scope": "openid profile offline_access email",
            },
            "accounts": [{"uid": uid, "nickname": nick}],
            "allAccounts": [{"uid": uid, "nickname": nick}],
        })
        .to_string()
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wb-scan-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn discovery_reads_current_snapshots_and_backups_without_domain() {
        let auth = temp_dir("auth");
        let backups = temp_dir("backup");
        let now = now_ms();
        let far = now + 30 * 24 * 3600 * 1000;

        // 国服当前登录（客户端写入时带 domain）
        std::fs::write(
            auth.join("workbuddy-desktop.info"),
            json!({
                "account": {"uid": "cn-1", "nickname": "国服甲"},
                "auth": {"accessToken": "AT-CN", "refreshToken": "RT-CN",
                         "expiresAt": far, "refreshExpiresAt": far,
                         "domain": "www.workbuddy.cn"},
            })
            .to_string(),
        )
        .unwrap();
        // 国际版历史快照
        std::fs::write(
            auth.join("workbuddy-desktop-ai.2026-09-12T07-58-10-771Z.6072.abc.info"),
            auth_payload("ai-1", "国际乙", "AT-AI", "RT-AI", far, far),
        )
        .unwrap();
        // 本工具备份（无 domain，天然带时间戳后缀）
        std::fs::write(
            backups.join("workbuddy-desktop.2026-08-31T05-24-57Z.info"),
            auth_payload("cn-2", "国服丙", "AT-BK", "RT-BK", far, far),
        )
        .unwrap();
        // 无关文件必须被忽略
        std::fs::write(
            auth.join("Tencent-Cloud.coding-copilot.2026-09-09T05-19-16-805Z.info"),
            auth_payload("other", "别家", "AT-X", "RT-X", far, far),
        )
        .unwrap();

        let found = discover_local_accounts_in(&auth, &backups);
        let names: Vec<String> = found
            .iter()
            .map(|c| get_str(&c.account, "nickname").unwrap_or_default())
            .collect();
        assert_eq!(found.len(), 3, "应发现 3 个账号，实际：{names:?}");
        assert!(!names.contains(&"别家".to_string()), "无关文件不得入库");

        let cn2 = found.iter().find(|c| c.file_name.starts_with("workbuddy-desktop.2026")).expect("备份应被发现");
        assert_eq!(cn2.source, AuthFileSource::Backup);
        assert_eq!(
            get_str(&cn2.account, "domain").as_deref(),
            Some("www.workbuddy.cn"),
            "备份缺 domain 时必须按文件名回填区域，否则国际版会被误记为国服"
        );

        let ai1 = found.iter().find(|c| c.region == Region::Intl).expect("国际版快照应被发现");
        assert_eq!(ai1.source, AuthFileSource::Snapshot);
        assert_eq!(get_str(&ai1.account, "domain").as_deref(), Some("www.workbuddy.ai"));

        std::fs::remove_dir_all(&auth).ok();
        std::fs::remove_dir_all(&backups).ok();
    }

    #[test]
    fn duplicate_snapshots_keep_the_freshest_credential() {
        let auth = temp_dir("dupe-auth");
        let backups = temp_dir("dupe-backup");
        let now = now_ms();
        let stale = now - 1000; // 已过期
        let fresh = now + 30 * 24 * 3600 * 1000;

        // 同一账号三份文件：旧的已过期、较新的可保活、当前登录的最旧。
        std::fs::write(
            auth.join("workbuddy-desktop.2026-08-30T08-40-52-204Z.7476.old.info"),
            auth_payload("uid-1", "某人", "AT-OLD", "RT-OLD", far_past(now), far_past(now)),
        )
        .unwrap();
        std::fs::write(
            auth.join("workbuddy-desktop.2026-09-10T08-40-52-204Z.7476.new.info"),
            auth_payload("uid-1", "某人", "AT-NEW", "RT-NEW", fresh, fresh),
        )
        .unwrap();
        std::fs::write(
            backups.join("workbuddy-desktop.2026-08-01T00-00-00Z.info"),
            auth_payload("uid-1", "某人", "AT-BK", "RT-BK", stale, stale),
        )
        .unwrap();

        let found = discover_local_accounts_in(&auth, &backups);
        assert_eq!(found.len(), 1, "同区域同 uid 只保留一份");
        assert_eq!(
            get_str(&found[0].account, "access_token").as_deref(),
            Some("AT-NEW"),
            "必须保留凭证最新的那份，否则会导入失效的 refresh token"
        );
        assert_eq!(found[0].freshness, CredentialFreshness::Refreshable);
        assert_eq!(found[0].duplicate_count, 3, "应报告本机共有 3 份同账号文件");

        std::fs::remove_dir_all(&auth).ok();
        std::fs::remove_dir_all(&backups).ok();
    }

    #[test]
    fn same_uid_in_different_regions_is_kept_separate() {
        let auth = temp_dir("region-auth");
        let backups = temp_dir("region-backup");
        let now = now_ms();
        let far = now + 30 * 24 * 3600 * 1000;

        // 国服与国际版可以有同 uid（命名空间独立），不得互相覆盖。
        std::fs::write(
            auth.join("workbuddy-desktop.2026-09-01T00-00-00Z.1.a.info"),
            auth_payload("shared-uid", "国服", "AT-CN", "RT-CN", far, far),
        )
        .unwrap();
        std::fs::write(
            auth.join("workbuddy-desktop-ai.2026-09-02T00-00-00Z.1.b.info"),
            auth_payload("shared-uid", "国际", "AT-AI", "RT-AI", far, far),
        )
        .unwrap();

        let found = discover_local_accounts_in(&auth, &backups);
        assert_eq!(found.len(), 2, "跨区域同 uid 必须各留一份");

        std::fs::remove_dir_all(&auth).ok();
        std::fs::remove_dir_all(&backups).ok();
    }

    #[test]
    fn credential_freshness_distinguishes_expired_from_refreshable() {
        let now = 1_800_000_000_000;
        let past = now - 1000;
        let future = now + 1000;

        let refreshable = json!({"access_token": "a", "refresh_token": "r",
                                 "expiresAt": past, "refreshExpiresAt": future});
        assert_eq!(
            credential_freshness(&refreshable, now),
            CredentialFreshness::Refreshable
        );

        // refresh 已过期但 access 仍有效
        let access_only = json!({"access_token": "a", "refresh_token": "r",
                                 "expiresAt": future, "refreshExpiresAt": past});
        assert_eq!(
            credential_freshness(&access_only, now),
            CredentialFreshness::AccessOnly
        );
        assert!(credential_freshness(&access_only, now).usable());

        // 全部过期
        let expired = json!({"access_token": "a", "refresh_token": "r",
                             "expiresAt": past, "refreshExpiresAt": past});
        assert_eq!(credential_freshness(&expired, now), CredentialFreshness::Expired);
        assert!(!credential_freshness(&expired, now).usable());

        // 缺 refresh token：只剩 access 能撑
        let no_rt = json!({"access_token": "a", "expiresAt": future});
        assert_eq!(credential_freshness(&no_rt, now), CredentialFreshness::AccessOnly);

        // 无到期时间的 refresh token 不应被当作过期（服务端才是裁判）
        let unknown = json!({"access_token": "a", "refresh_token": "r"});
        assert_eq!(
            credential_freshness(&unknown, now),
            CredentialFreshness::Refreshable
        );
    }

    /// `now` 之前的绝对时间戳（毫秒）。
    fn far_past(now: i64) -> i64 {
        now - 24 * 3600 * 1000
    }
}
