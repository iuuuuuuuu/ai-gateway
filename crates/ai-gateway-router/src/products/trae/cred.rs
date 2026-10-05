//! Trae 网关凭证的解析与扫描（`~/.wb-switch/trae/auths/trae-<user_id>.json`）。
//!
//! # 这份文件是**宿主写、网关读**
//!
//! 宿主侧 `ai-gateway-core::modules::trae_account` 拥有账号库
//! （`~/.wb-switch/trae_accounts.json`），网关**从不读它** —— 网关的账号池是
//! **扫描凭证目录**建立的（与 Qoder / ZCode 完全同构）。
//! 所以宿主每次写账号库之后必须同步一份到这里，否则表现为
//! 「界面上账号加好了、状态也正常，但请求永远不经过它」。
//!
//! # 为什么必须带 `account` 顶层对象
//!
//! 宿主 UI 上有个「停止接流量」开关，它**只改宿主自己的库**是无效的
//! （网关根本不读那个库）。真正生效的通道是往凭证文件里写 `account.no_route`，
//! 网关的 `Pool::healthy()` 会因此排除该账号。形状与 Qoder / ZCode 的
//! `credential_no_route` 读取路径一致。
//!
//! # 字段名为什么是 `auth.token` 而不是 `auth.accessToken`
//!
//! `crate::auth::Auth` 的 `NestedAuth`（`accessToken`/`refreshToken`/`expiresAt`）
//! 是 **WorkBuddy 插件 OAuth** 的形状，产品凭证各写各的。Trae 用 `token` ——
//! 因为真实凭证里这个值是 `Cloud-IDE-JWT <jwt>` 整串（不是裸 JWT），
//! 叫 `accessToken` 会让人以为它与 WorkBuddy 同构。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::realm::{self, CredentialHint, Realm, RealmEvidence};
use crate::auth::{Auth, PRODUCT_TRAE};

/// 凭证文件名前缀。网关只扫 `trae*.json`，与 Qoder/ZCode 的扫描口径一致。
pub const CRED_PREFIX: &str = "trae-";

/// 一个 Trae 账号的网关侧凭证。
#[derive(Debug, Clone, PartialEq)]
pub struct TraeCred {
    /// 网关账号池里的 uid，**必须**是 `trae-<user_id>`。
    ///
    /// `usage.rs` 的 `product_from_uid` 靠这个前缀把用量归到 Trae 名下；
    /// 丢了前缀会让 Trae 的用量显示成 WorkBuddy 的。
    pub uid: String,
    /// 宿主账号库里的裸 `user_id`（纯数字，如 `2593120869167228`）。
    pub user_id: String,
    pub nickname: String,
    /// `Cloud-IDE-JWT <jwt>` 整串。
    pub token: String,
    pub refresh_token: String,
    /// access token 到期时刻（Unix 秒）。`0` = 未知。
    pub expires_at: i64,
    /// 区域（显式值优先，缺失时按 host/region 探测）。
    pub realm: Realm,
    /// 区域是怎么判出来的（日志用）。
    pub realm_evidence: RealmEvidence,
    /// 宿主写入的 `userRegion` 区域码（`CN` / `SG` / `US` …），空 = 未知。
    ///
    /// 只用于一件事：`SG` 区域且区域码为 `US` 时 chat host 换成
    /// `realm::US_CHAT_HOST`（见 `realm::chat_host`）。丢了这个字段，
    /// 美区账号会被打到 `coresg-normal.trae.ai` 上。
    pub region_code: String,
    /// 宿主写入的 `account.no_route`（用户手动停用）。
    pub no_route: bool,
    /// 来源文件路径（刷新后原子写回用）。
    pub file_path: String,
}

/// 磁盘上的嵌套形状。
///
/// ★ 每个字段都 `#[serde(default)]`：凭证是**跨进程契约**，宿主与网关版本可能
/// 不同步（网关随宿主内嵌，但用户可能只更新了一边）。缺字段时取默认值继续，
/// 比整个文件解析失败好 —— 后者会让账号**静默消失**。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct NestedForm {
    #[serde(default)]
    account: Option<NestedAccount>,
    #[serde(default)]
    auth: Option<NestedAuth>,
    /// 顶层 `uid`（早期手工导入的凭证可能这么写）。
    #[serde(default)]
    uid: Option<String>,
    /// 顶层 `user_id`（宿主账号库的字段名）。
    #[serde(default)]
    user_id: Option<String>,
    /// 顶层 `jwt`（宿主账号库的字段名）。
    #[serde(default)]
    jwt: Option<String>,
    /// 顶层 `refresh_token`（宿主账号库的字段名）。
    #[serde(default)]
    refresh_token: Option<String>,
    // ── 顶层同名区域线索 ──
    //
    // ★ flat 形状（`trae_accounts.json` 的数组元素）就是这一层：
    //   `{"user_id":"...","jwt":"...","_edition":"cn","user_region":"CN"}`。
    //   core 的 `trae_account::account_realm` 收到的是**整个**记录，故这些键
    //   在 core 侧是顶层；网关读的导出副本可能把它包进 `account`，两种都要认。
    #[serde(default)]
    _edition: Option<String>,
    #[serde(default)]
    realm: Option<String>,
    #[serde(default)]
    user_region: Option<Value>,
    #[serde(default, rename = "userRegion")]
    user_region_camel: Option<Value>,
    #[serde(default)]
    auth_host: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct NestedAccount {
    #[serde(default)]
    uid: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    nickname: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    no_route: Option<bool>,
    // ── 与宿主账号记录同名的区域线索（trae_account.rs:465-483 逐字）──
    /// 宿主写下的区域别名（realms.js 用的键名）。
    #[serde(default)]
    _edition: Option<String>,
    /// 宿主写下的显式区域。
    #[serde(default)]
    realm: Option<String>,
    /// userRegion / user_region：对象取 .region，也可能直接是字符串。
    #[serde(default)]
    user_region: Option<Value>,
    #[serde(default, rename = "userRegion")]
    user_region_camel: Option<Value>,
    /// 该凭证打过的 host（老记录可能没有）。
    #[serde(default)]
    auth_host: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct NestedAuth {
    /// `Cloud-IDE-JWT <jwt>` 整串。
    #[serde(default)]
    token: Option<String>,
    /// 兼容别名。
    #[serde(default)]
    jwt: Option<String>,
    #[serde(default, rename = "accessToken")]
    access_token: Option<String>,
    #[serde(default, rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(default)]
    refresh_token_snake: Option<String>,
    #[serde(default, rename = "expiresAt")]
    expires_at: Option<i64>,
    /// 区域（宿主导出时显式写入）。
    #[serde(default)]
    realm: Option<String>,
    /// 兼容别名 `_edition`（realms.js 用的键名）。
    #[serde(default, rename = "_edition")]
    edition: Option<String>,
    /// 与上同义的另一种写法（老宿主写过 `edition`）。
    #[serde(default)]
    edition_bare: Option<String>,
    /// 兼容别名：userRegion（可能是对象取 `.region`，也可能直接是字符串）。
    #[serde(default)]
    user_region: Option<Value>,
    /// 该凭证是打哪个 host 拿到的（区域探测线索）。
    #[serde(default)]
    auth_host: Option<String>,
}

fn first_non_empty(candidates: &[Option<&String>]) -> String {
    for c in candidates {
        if let Some(s) = c {
            let t = s.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    String::new()
}

/// 从 `userRegion` 字段里抠区域码。
///
/// 宿主侧 `trae_account::account_realm` 的注释说得很清楚：这个键
/// **可能是对象取 `.region`，也可能直接是字符串** —— 两种都要认，
/// 只认一种会在另一种形态上静默退化成兜底猜测。
fn region_code_of(v: Option<&Value>) -> Option<String> {
    let v = v?;
    if let Some(s) = v.as_str() {
        let t = s.trim();
        if t.is_empty() {
            return None;
        }
        return Some(t.to_string());
    }
    let r = v.get("region")?.as_str()?.trim();
    if r.is_empty() {
        None
    } else {
        Some(r.to_string())
    }
}

impl TraeCred {
    /// 从已解析的 JSON 构造。
    pub fn from_value(doc: &Value, file_path: &str) -> Result<Self, String> {
        let form: NestedForm =
            serde_json::from_value(doc.clone()).map_err(|e| format!("结构不匹配：{e}"))?;
        let acct = form.account.clone().unwrap_or_default();
        let auth = form.auth.clone().unwrap_or_default();

        let raw_uid = first_non_empty(&[acct.uid.as_ref(), form.uid.as_ref()]);
        // account.uid 若是裸数字，它本身就是 user_id。
        let uid_as_user_id = if raw_uid.starts_with(CRED_PREFIX) {
            None
        } else {
            Some(&raw_uid)
        };
        // ★ uid 带前缀、而记录里没写 user_id 时，前缀后的那一段**就是** user_id。
        //   x-uid 请求头用的是裸 user_id（headers.rs），推导不出来就会整个头不发、
        //   上游直接拒。core 的 account_realm 也只从落盘的 user_id 取，故以磁盘为准。
        let from_prefixed_uid = raw_uid.trim().strip_prefix(CRED_PREFIX);
        let user_id = first_non_empty(&[
            acct.user_id.as_ref(),
            form.user_id.as_ref(),
            uid_as_user_id,
        ]);
        let user_id = if user_id.is_empty() {
            from_prefixed_uid.unwrap_or_default().to_string()
        } else {
            user_id
        };

        // 归一：无论磁盘上写的是 "trae-123" 还是 "123"，uid 都要带前缀。
        let uid = normalize_uid(&raw_uid, &user_id);
        if uid.is_empty() {
            return Err("缺少账号标识（uid / user_id 都为空）".to_string());
        }

        let token = first_non_empty(&[
            auth.token.as_ref(),
            auth.jwt.as_ref(),
            auth.access_token.as_ref(),
            form.jwt.as_ref(),
        ]);
        if token.is_empty() {
            return Err("缺少 access token".to_string());
        }

        let refresh_token = first_non_empty(&[
            auth.refresh_token.as_ref(),
            auth.refresh_token_snake.as_ref(),
            form.refresh_token.as_ref(),
        ]);

        let nickname = first_non_empty(&[acct.nickname.as_ref(), acct.name.as_ref()]);
        let expires_at = auth.expires_at.unwrap_or(0);

        // ── 区域判定 ──
        //
        // ★ 线索优先级与 core 的 trae_account::account_realm 一致：
        //   显式（_edition / realm）→ auth 段 → account 段 → userRegion → auth_host → 兜底。
        //   两边都要认：宿主把区域写在**账号记录顶层**，而网关读的是导出副本。
        let explicit = first_non_empty(&[
            auth.realm.as_ref(),
            auth.edition.as_ref(),
            auth.edition_bare.as_ref(),
            acct.realm.as_ref(),
            acct._edition.as_ref(),
            form.realm.as_ref(),
            form._edition.as_ref(),
        ]);
        let region = region_code_of(auth.user_region.as_ref())
            .or_else(|| region_code_of(acct.user_region.as_ref()))
            .or_else(|| region_code_of(acct.user_region_camel.as_ref()))
            .or_else(|| region_code_of(form.user_region.as_ref()))
            .or_else(|| region_code_of(form.user_region_camel.as_ref()));
        let guess = realm::realm_from_credential(&CredentialHint {
            explicit: if explicit.is_empty() {
                None
            } else {
                Some(explicit.as_str())
            },
            host: auth
                .auth_host
                .as_deref()
                .or(acct.auth_host.as_deref())
                .or(form.auth_host.as_deref()),
            issuer: None,
            region: region.as_deref(),
        });

        Ok(TraeCred {
            uid,
            user_id,
            nickname,
            token,
            refresh_token,
            expires_at,
            realm: guess.realm,
            realm_evidence: guess.evidence,
            // ★ 只有在区域判定**来自 userRegion** 时才保留美区节点信息：
            //   显式区域（_edition）与区域码可能互相矛盾，而 host 选择只能有一个
            //   依据。留空 = 用该区域的默认 host（CN 永远不该去 coreva）。
            region_code: if guess.evidence == RealmEvidence::UserRegion {
                region.unwrap_or_default()
            } else {
                String::new()
            },
            no_route: acct.no_route.unwrap_or(false),
            file_path: file_path.to_string(),
        })
    }

    /// 从文件加载。
    pub fn load_file(path: &str) -> Result<Self, String> {
        let raw = std::fs::read(path).map_err(|e| format!("读取失败：{e}"))?;
        let doc: Value = serde_json::from_slice(&raw).map_err(|e| format!("不是合法 JSON：{e}"))?;
        Self::from_value(&doc, path)
    }

    /// 转成账号池里的 `Auth`。
    ///
    /// ★ `domain` **刻意留空**。Trae 的区域语义与 WorkBuddy 的
    /// `region_of_domain` 完全不同：`crate::auth::INTL_DOMAINS` 里根本没有 trae
    /// 域名，写上 `trae.ai` 只会让 `is_intl()` 依赖一张不该管它的表；
    /// 更糟的是 `scheduler`（`checkin_scope != "all"` 时跳过 `is_intl()` 的账号）
    /// 与 `growtask/runner.rs` 会把它当 WorkBuddy 国际版账号**静默跳过**。
    /// 区域信息由 `super::dispatch` 从本结构体的 `realm` 字段读，不走 `Auth::region()`。
    pub fn to_auth(&self) -> Auth {
        Auth {
            access_token: self.token.clone(),
            refresh_token: self.refresh_token.clone(),
            expires_at: self.expires_at,
            domain: String::new(),
            uid: self.uid.clone(),
            enterprise_id: String::new(),
            nickname: self.nickname.clone(),
            product: PRODUCT_TRAE.to_string(),
            no_route: self.no_route,
            file_path: self.file_path.clone(),
            soonest_expire_at: 0,
        }
    }

    /// 把刷新结果写回凭证文件。
    ///
    /// ★ **先读回磁盘再合并**（不只用内存字段重建），理由与
    /// `products/qoder/cred.rs::save_atomic` 完全相同：宿主会往同一份文件写
    /// `account.no_route`，网关整体覆盖会在几分钟内**悄悄抹掉用户的禁用标记** ——
    /// 用户看到的现象是「点了停止接流量，过一会儿它又自己开始接」。
    pub fn save_refreshed(
        &self,
        access_token: &str,
        refresh_token: Option<&str>,
        expires_at: i64,
    ) -> Result<(), String> {
        if self.file_path.is_empty() {
            return Err("没有来源文件路径".to_string());
        }
        let mut doc: Value = std::fs::read(&self.file_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        if !doc.is_object() {
            doc = Value::Object(serde_json::Map::new());
        }
        let root = doc.as_object_mut().expect("上面已保证是对象");

        let auth = root
            .entry("auth".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if !auth.is_object() {
            *auth = Value::Object(serde_json::Map::new());
        }
        let auth = auth.as_object_mut().expect("上面已保证是对象");
        auth.insert("token".to_string(), Value::String(access_token.to_string()));
        if let Some(rt) = refresh_token {
            if !rt.trim().is_empty() {
                auth.insert(
                    "refreshToken".to_string(),
                    Value::String(rt.trim().to_string()),
                );
            }
        }
        if expires_at > 0 {
            auth.insert("expiresAt".to_string(), Value::Number(expires_at.into()));
        }
        // 区域是**已判定的事实**，写回去让下一次加载走「显式」分支，
        // 不再依赖 issuer 探测（实盘 JWT 没有 iss）。
        auth.insert(
            "realm".to_string(),
            Value::String(self.realm.as_str().to_string()),
        );

        let text = serde_json::to_string_pretty(&doc).map_err(|e| format!("序列化失败：{e}"))?;
        write_atomic(&self.file_path, &text)
    }
}

/// 归一 uid：无论入参是 `123`、`trae-123` 还是空的，都产出带前缀的形态。
pub fn normalize_uid(raw_uid: &str, user_id: &str) -> String {
    let base = if !user_id.trim().is_empty() {
        user_id.trim()
    } else {
        raw_uid.trim()
    };
    if base.is_empty() {
        return String::new();
    }
    if base.starts_with(CRED_PREFIX) {
        base.to_string()
    } else {
        format!("{CRED_PREFIX}{base}")
    }
}

/// 凭证文件名：`trae-<sanitized>.json`。
///
/// 与 `zcode_account::cred_file_name` 同款：uid 已带前缀就**不再拼一次**
/// （否则得到 `trae-trae-xxx.json`，Go 侧实测踩过这个坑）。
pub fn cred_file_name(uid: &str) -> String {
    let t = uid.trim();
    let base = if t.starts_with(CRED_PREFIX) {
        t.to_string()
    } else {
        format!("{CRED_PREFIX}{t}")
    };
    format!("{}.json", sanitize_uid(&base))
}

/// 把路径分隔符等替换掉，防止 `../` 之类的 uid 写到目录外。
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

/// 原子写：先写 `.tmp` 再 rename。
fn write_atomic(path: &str, text: &str) -> Result<(), String> {
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写临时文件失败：{e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("替换凭证文件失败：{e}"))
}

/// 扫描目录里的全部 Trae 凭证。
///
/// 与 Qoder/ZCode 的 `load_dir` 同款口径：
/// - **目录不存在等价于「没有凭证」**，返回空且不报错（多产品未启用时是常态）；
/// - 逐条报错**不静默跳过**（一条坏凭证不该让整个目录静默少一个账号）；
/// - 文件名排序保证结果稳定。
pub fn load_dir(dir: &str) -> (Vec<TraeCred>, Vec<String>) {
    let mut out = Vec::new();
    let mut failed = Vec::new();
    if dir.trim().is_empty() {
        return (out, failed);
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return (out, failed), // 目录不存在 / 不可读 = 没有凭证
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("trae") && n.ends_with(".json"))
                    .unwrap_or(false)
        })
        .collect();
    paths.sort();
    for p in paths {
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        match TraeCred::load_file(&p.to_string_lossy()) {
            Ok(c) => out.push(c),
            Err(e) => failed.push(format!("{name}: {e}")),
        }
    }
    (out, failed)
}

/// 默认凭证目录：`~/.wb-switch/trae/auths`。
///
/// 与 `config::default_trae_auth_dir` 一致（那边用 `home_join` 拼；
/// 这里重复一份是为了让本模块能独立测试，且不把 `config` 拖进来）。
pub fn default_auth_dir() -> String {
    let home = std::env::var("USERPROFILE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| std::env::var("HOME").ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_default();
    if home.is_empty() {
        return String::new();
    }
    std::path::Path::new(&home)
        .join(".wb-switch")
        .join("trae")
        .join("auths")
        .to_string_lossy()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(json: &str) -> Value {
        serde_json::from_str(json).expect("测试 JSON 必须合法")
    }

    /// 宿主导出的**规范形态**（`trae_account::sync_credential` 的产物）。
    const CANONICAL: &str = r#"{
      "account": { "uid": "trae-2593120869167228", "user_id": "2593120869167228", "nickname": "主号" },
      "auth": {
        "token": "Cloud-IDE-JWT eyJhbGciOiJSUzI1NiJ9.payload.sig",
        "refreshToken": "U0-bMqJAoQ0JOIbXIJg16PHsvRf2rpr77pxWKru8BH4=.18da9ccc9428ed1c",
        "expiresAt": 1792125804,
        "realm": "cn"
      }
    }"#;

    #[test]
    fn parses_the_canonical_shape() {
        let c = TraeCred::from_value(&doc(CANONICAL), "C:/x/trae-2593120869167228.json").unwrap();
        assert_eq!(c.uid, "trae-2593120869167228");
        assert_eq!(c.user_id, "2593120869167228");
        assert_eq!(c.nickname, "主号");
        assert_eq!(c.expires_at, 1792125804);
        assert_eq!(c.realm, Realm::Cn);
        assert_eq!(c.realm_evidence, RealmEvidence::Explicit);
        assert!(!c.no_route);
        assert!(c.token.starts_with("Cloud-IDE-JWT "));
        assert_eq!(
            c.refresh_token,
            "U0-bMqJAoQ0JOIbXIJg16PHsvRf2rpr77pxWKru8BH4=.18da9ccc9428ed1c"
        );
    }

    #[test]
    fn to_auth_keeps_domain_empty_on_purpose() {
        let c = TraeCred::from_value(&doc(CANONICAL), "p").unwrap();
        let a = c.to_auth();
        assert_eq!(a.uid, "trae-2593120869167228");
        assert_eq!(a.product, PRODUCT_TRAE);
        assert!(a.is_trae());
        // 空 domain ⇒ Region::CN。写上 trae.ai 会让 scheduler/growtask
        // 把它当 WorkBuddy 国际版账号静默跳过。
        assert_eq!(a.domain, "");
        assert!(!a.is_intl());
        assert_eq!(a.expires_at, 1792125804);
    }

    #[test]
    fn a_bare_numeric_uid_still_gets_the_prefix() {
        let c = TraeCred::from_value(
            &doc(r#"{"account":{"uid":"2593120869167228"},"auth":{"token":"Cloud-IDE-JWT x"}}"#),
            "p",
        )
        .unwrap();
        assert_eq!(c.uid, "trae-2593120869167228");
        assert_eq!(c.user_id, "2593120869167228");
    }

    #[test]
    fn a_prefixed_uid_is_not_prefixed_twice() {
        let c = TraeCred::from_value(
            &doc(r#"{"account":{"uid":"trae-123"},"auth":{"token":"t"}}"#),
            "p",
        )
        .unwrap();
        assert_eq!(c.uid, "trae-123");
        assert_eq!(c.user_id, "123");
        assert_eq!(cred_file_name(&c.uid), "trae-123.json");
    }

    /// 兼容形态：宿主账号库里的字段名（jwt / refresh_token）直接搬过来。
    #[test]
    fn accepts_the_flat_host_account_shape() {
        let c = TraeCred::from_value(
            &doc(r#"{"user_id":"999","jwt":"Cloud-IDE-JWT y","refresh_token":"U0-rt"}"#),
            "p",
        )
        .unwrap();
        assert_eq!(c.uid, "trae-999");
        assert_eq!(c.token, "Cloud-IDE-JWT y");
        assert_eq!(c.refresh_token, "U0-rt");
        // 没有区域线索 ⇒ 兜底 cn（与宿主默认一致）。
        assert_eq!(c.realm, Realm::Cn);
        assert_eq!(c.realm_evidence, RealmEvidence::Default);
    }

    #[test]
    fn realm_can_come_from_an_alias_or_a_region_object() {
        let c = TraeCred::from_value(
            &doc(r#"{"account":{"uid":"1"},"auth":{"token":"t"},"_edition":"sg"}"#),
            "p",
        )
        .unwrap();
        assert_eq!(c.realm, Realm::Sg);
        assert_eq!(c.realm_evidence, RealmEvidence::Explicit);

        // userRegion 是**对象**取 .region
        let c = TraeCred::from_value(
            &doc(r#"{"account":{"uid":"1"},"user_region":{"region":"US"},"auth":{"token":"t"}}"#),
            "p",
        )
        .unwrap();
        assert_eq!(c.realm, Realm::Sg);
        assert_eq!(c.realm_evidence, RealmEvidence::UserRegion);

        // userRegion 直接是**字符串**
        let c = TraeCred::from_value(
            &doc(r#"{"account":{"uid":"1"},"user_region":"CN","auth":{"token":"t"}}"#),
            "p",
        )
        .unwrap();
        assert_eq!(c.realm, Realm::Cn);
        assert_eq!(c.realm_evidence, RealmEvidence::UserRegion);
    }

    #[test]
    fn a_stale_region_code_never_sends_a_cn_token_to_the_us_host() {
        // ★ 实测过的坑：宿主的 user_region 是**账号属性**，而 _edition 是路由选择，
        //   两者会互相矛盾（用户先登国服、后来账号记录里留着 userRegion=US）。
        //   若把 US 当依据，国服的令牌会被发去 coreva（美区）主机 ⇒ 上游 4017 风控。
        let c = TraeCred::from_value(
            &doc(
                r#"{"account":{"uid":"1"},"_edition":"cn","user_region":"US","auth":{"token":"t"}}"#,
            ),
            "p",
        )
        .unwrap();
        assert_eq!(c.realm, Realm::Cn);
        assert_eq!(c.realm_evidence, RealmEvidence::Explicit);
        assert_eq!(c.region_code, "");
        assert_eq!(realm::chat_host(c.realm, &c.region_code), "https://trae-api-cn.mchost.guru");

        // 反过来：区域判定**来自 userRegion** 时才认美区节点。
        let c = TraeCred::from_value(
            &doc(r#"{"account":{"uid":"1"},"user_region":"US","auth":{"token":"t"}}"#),
            "p",
        )
        .unwrap();
        assert_eq!(c.realm, Realm::Sg);
        assert_eq!(c.region_code, "US");
        assert_eq!(realm::chat_host(c.realm, &c.region_code), "https://coreva-normal.trae.ai");
    }

    #[test]
    fn no_route_is_read_from_the_account_object() {
        let c = TraeCred::from_value(
            &doc(r#"{"account":{"uid":"1","no_route":true},"auth":{"token":"t"}}"#),
            "p",
        )
        .unwrap();
        assert!(c.no_route);
        assert!(c.to_auth().no_route);
    }

    #[test]
    fn missing_uid_or_token_is_an_error_not_a_default() {
        assert!(TraeCred::from_value(&doc(r#"{"auth":{"token":"t"}}"#), "p").is_err());
        assert!(TraeCred::from_value(&doc(r#"{"account":{"uid":"1"}}"#), "p").is_err());
        assert!(
            TraeCred::from_value(&doc(r#"{"account":{"uid":"  "},"auth":{"token":"t"}}"#), "p")
                .is_err()
        );
    }

    #[test]
    fn sanitize_uid_blocks_path_traversal() {
        assert_eq!(sanitize_uid("a/b"), "a_b");
        assert_eq!(sanitize_uid(".."), "__");
        assert_eq!(sanitize_uid("a..b"), "a__b");
        assert_eq!(sanitize_uid(""), "unknown");
        assert_eq!(cred_file_name("trae-1"), "trae-1.json");
        assert_eq!(cred_file_name("1"), "trae-1.json");
    }

    #[test]
    fn load_dir_reports_a_missing_directory_as_empty() {
        let (creds, failed) = load_dir("D:/definitely-not-a-real-dir-placeholder");
        assert!(creds.is_empty());
        assert!(failed.is_empty());
        let (creds, failed) = load_dir("");
        assert!(creds.is_empty() && failed.is_empty());
    }

    #[test]
    fn load_dir_reads_only_trae_json_files() {
        let dir = std::env::temp_dir().join(format!("wb2api-trae-cred-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("trae-1.json"), CANONICAL).unwrap();
        // 非 trae 前缀 / 非 .json：必须被忽略
        std::fs::write(dir.join("qoder-9.json"), "{}").unwrap();
        std::fs::write(dir.join("trae-2.txt"), "{}").unwrap();
        // 坏凭证：进 failed，不进 creds
        std::fs::write(dir.join("trae-3.json"), "{ not json").unwrap();

        let (creds, failed) = load_dir(&dir.to_string_lossy());
        assert_eq!(creds.len(), 1, "只应加载 trae-1.json");
        assert_eq!(creds[0].uid, "trae-2593120869167228");
        assert_eq!(failed.len(), 1);
        assert!(failed[0].starts_with("trae-3.json: "), "failed={failed:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 刷新写回必须**保住宿主写的 no_route** —— 整体覆盖会让用户的
    /// 「停止接流量」在几分钟后自己失效。
    #[test]
    fn save_refreshed_preserves_host_written_fields() {
        let dir = std::env::temp_dir().join(format!("wb2api-trae-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trae-1.json");
        std::fs::write(
            &path,
            r#"{"account":{"uid":"trae-1","user_id":"1","no_route":true,"nickname":"主号"},
                "auth":{"token":"old","refreshToken":"U0-old","expiresAt":1,"realm":"cn"},
                "extra_key_from_host":"must survive"}"#,
        )
        .unwrap();

        let c = TraeCred::load_file(&path.to_string_lossy()).unwrap();
        assert!(c.no_route);
        c.save_refreshed("new-token", Some("U0-new"), 12345).unwrap();

        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["account"]["no_route"], Value::Bool(true), "no_route 必须保留");
        assert_eq!(
            back["extra_key_from_host"],
            Value::String("must survive".into())
        );
        assert_eq!(back["auth"]["token"], Value::String("new-token".into()));
        assert_eq!(back["auth"]["refreshToken"], Value::String("U0-new".into()));
        assert_eq!(back["auth"]["expiresAt"], Value::Number(12345.into()));

        let c2 = TraeCred::load_file(&path.to_string_lossy()).unwrap();
        assert_eq!(c2.token, "new-token");
        assert_eq!(c2.refresh_token, "U0-new");
        assert!(c2.no_route, "no_route 必须能被重新读出");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_refreshed_keeps_the_old_refresh_token_when_none_is_returned() {
        let dir = std::env::temp_dir().join(format!("wb2api-trae-save2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trae-1.json");
        std::fs::write(
            &path,
            r#"{"account":{"uid":"trae-1"},"auth":{"token":"old","refreshToken":"U0-old","realm":"cn"}}"#,
        )
        .unwrap();
        let c = TraeCred::load_file(&path.to_string_lossy()).unwrap();
        c.save_refreshed("new-token", None, 0).unwrap();
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["auth"]["refreshToken"], Value::String("U0-old".into()));
        let c2 = TraeCred::load_file(&path.to_string_lossy()).unwrap();
        assert_eq!(c2.token, "new-token");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_auth_dir_points_under_wb_switch() {
        let d = default_auth_dir();
        if !d.is_empty() {
            assert!(
                d.replace('\\', "/").ends_with(".wb-switch/trae/auths"),
                "got {d}"
            );
        }
    }
}
