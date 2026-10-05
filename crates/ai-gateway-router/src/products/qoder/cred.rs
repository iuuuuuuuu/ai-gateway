//! Qoder 凭证：三种形态解析、原子保存、机器指纹。
//!
//! 逐条移植自 Go `internal/qoder/cred.go`。差异只在语言层面：
//! Go 用 `mu sync.Mutex` 让凭证可在刷新令牌时被并发改，Rust 侧由调用方
//! 持有 `&mut`，故不引入锁。
//!
//! # 与 WorkBuddy 凭证的差别（Go 包注释原文）
//!
//! | 维度     | WorkBuddy                | Qoder                                    |
//! |----------|--------------------------|------------------------------------------|
//! | 鉴权     | `Bearer <accessToken>`   | COSY 签名（见 `cosy.rs`）                |
//! | 令牌     | accessToken + refreshToken | `dt-`（约 30 天）+ `drt-`（约 1 年，轮换） |
//! | 指纹     | 无                       | **必需** MachineID/MachineToken/MachineType |
//! | 授权     | OAuth 授权码             | PKCE 设备流                              |
//! | 端点     | `/v2/chat/completions`   | `/algo/api/v2/...`（签名时要去掉 `/algo`） |

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use super::region::{region_from_domain, QoderRegion};

/// Qoder 凭证。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QoderCred {
    /// 上游账号 ID。
    pub uid: String,
    /// 昵称。
    pub nickname: String,
    /// `dt-` 设备令牌（COSY 身份里叫 `security_oauth_token`）。
    pub dt: String,
    /// `drt-` 刷新令牌（每次刷新都会轮换）。
    pub drt: String,
    /// `dt` 到期时间（Unix 秒；<= 0 表示未知）。
    pub dt_expires_at: i64,
    /// 机器指纹 ID。
    pub machine_id: String,
    /// 机器指纹令牌。
    pub machine_token: String,
    /// 机器类型（桌面客户端固定 `"5"`）。
    pub machine_type: String,
    /// 区域。
    pub region: QoderRegion,
    /// 宿主账号页「停止接流量」开关写进来的标记。
    ///
    /// ⚠ 必须走**凭证文件**而不是宿主账号库：网关的池是扫描 `auths/` 目录
    /// 建立的，它根本不读宿主的 `accounts.json`；只改账号库的话界面开关
    /// 是摆设。命名与 WorkBuddy 侧一致（`no_route`），刻意不用 `disabled`
    /// —— 池自己也有一组 `disabled`。
    pub no_route: bool,
    /// 凭证文件路径（`save_atomic` 的目标）。
    pub file_path: String,
}

/// 嵌套形（OAuth 落盘形态）。
#[derive(Debug, Default, Deserialize)]
struct NestedForm {
    #[serde(default)]
    auth: Option<NestedAuth>,
    #[serde(default)]
    account: Option<NestedAccount>,
    #[serde(default)]
    machine: Option<NestedMachine>,
}

#[derive(Debug, Default, Deserialize)]
struct NestedAuth {
    #[serde(default, rename = "accessToken")]
    access_token: Option<String>,
    #[serde(default, rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(default, rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(default)]
    domain: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct NestedAccount {
    #[serde(default)]
    uid: Option<String>,
    #[serde(default)]
    nickname: Option<String>,
    #[serde(default)]
    no_route: bool,
}

/// ⚠ `machine` 段**必须读回来**：此前没读，于是 [`QoderCred::save_atomic`]
/// 写出的 machine 段下次加载被丢掉，[`QoderCred::ensure_fingerprint`] 又生成
/// 新的。上游把「同一账号 + 同一机器指纹」视为同一设备，每次重启换一组
/// 指纹会让上游看到「同一账号从大量不同设备登录」，可能触发风控。
#[derive(Debug, Default, Deserialize)]
struct NestedMachine {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default, rename = "type")]
    type_: Option<String>,
}

/// 扁平形（手写 / 旧版 / 本仓库宿主导出）。
#[derive(Debug, Default, Deserialize)]
struct FlatForm {
    #[serde(default)]
    token: Option<String>,
    #[serde(default, rename = "accessToken")]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default, rename = "refreshToken")]
    refresh_token2: Option<String>,
    #[serde(default)]
    expires_at_unix: Option<i64>,
    #[serde(default, rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(default)]
    uid: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    nickname: Option<String>,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    no_route: bool,
}

impl QoderCred {
    /// 解析凭证字节（`path` 只用于错误文案与文件名兜底 UID）。
    ///
    /// 三种形态：① 嵌套形（OAuth 落盘）；② 扁平形（手写/旧版）；
    /// ③ 本仓库宿主导出形（也是扁平形，只是键名不同）。
    /// 解析失败**一律返回 error，不静默跳过** —— 调用方要区分
    /// 「文件不存在」与「文件存在但格式不对」。
    pub fn parse(raw: &[u8], path: &str) -> Result<Self, String> {
        if raw.is_empty() {
            return Err(format!("凭证文件为空: {}", base_name(path)));
        }
        let probe: Map<String, Value> =
            serde_json::from_slice(raw).map_err(|e| format!("凭证不是合法 JSON: {e}"))?;

        let mut c = QoderCred {
            file_path: path.to_string(),
            ..Default::default()
        };

        if probe.contains_key("auth") {
            let n: NestedForm = serde_json::from_slice(raw)
                .map_err(|e| format!("嵌套形解析失败: {e}"))?;
            let auth = n.auth.unwrap_or_default();
            let account = n.account.unwrap_or_default();
            let machine = n.machine.unwrap_or_default();
            c.dt = auth.access_token.unwrap_or_default();
            c.drt = auth.refresh_token.unwrap_or_default();
            c.dt_expires_at = auth.expires_at.unwrap_or(0);
            c.uid = account.uid.unwrap_or_default();
            c.nickname = account.nickname.unwrap_or_default();
            c.no_route = account.no_route;
            c.machine_id = machine.id.unwrap_or_default();
            c.machine_token = machine.token.unwrap_or_default();
            c.machine_type = machine.type_.unwrap_or_default();
            c.region = region_from_domain(&auth.domain.unwrap_or_default());
        } else {
            let f: FlatForm = serde_json::from_slice(raw)
                .map_err(|e| format!("扁平形解析失败: {e}"))?;
            c.dt = first_non_empty(&[
                f.token.as_deref().unwrap_or(""),
                f.access_token.as_deref().unwrap_or(""),
            ]);
            c.drt = first_non_empty(&[
                f.refresh_token.as_deref().unwrap_or(""),
                f.refresh_token2.as_deref().unwrap_or(""),
            ]);
            c.dt_expires_at = first_non_zero(&[
                f.expires_at_unix.unwrap_or(0),
                f.expires_at.unwrap_or(0),
            ]);
            c.uid = first_non_empty(&[
                f.uid.as_deref().unwrap_or(""),
                f.user_id.as_deref().unwrap_or(""),
            ]);
            c.nickname = f.nickname.unwrap_or_default();
            c.no_route = f.no_route;
            c.region = region_from_domain(&f.domain.unwrap_or_default());
        }

        if c.dt.is_empty() && c.drt.is_empty() {
            return Err(format!(
                "凭证里既没有 accessToken 也没有 refreshToken: {}",
                base_name(path)
            ));
        }

        if c.uid.is_empty() {
            let base = base_name(path);
            for prefix in ["qoder-", "qoderwork-"] {
                if base.starts_with(prefix) && base.ends_with(".json") {
                    c.uid = base[prefix.len()..base.len() - ".json".len()].to_string();
                    break;
                }
            }
        }

        // ★ 未知区域**不猜**：凭证可能没有 domain，默认成国服会把国际版
        // 账号打到国服端点，必然失败。`region_from_domain` 已经返回
        // `Unknown`（枚举没有「空」这个值），这里**刻意不做任何回落** ——
        // 端点选择由 `dispatch::cred_of` 负责（那里才把 Unknown 当国服，
        // 且不写回 `Auth.domain`，用户仍可在界面上更正）。
        Ok(c)
    }

    /// 读文件并解析。文件不存在时返回 `std::io::Error` 的原文（与 Go 同口径）。
    pub fn load_file(path: &str) -> Result<Self, String> {
        let raw = std::fs::read(path).map_err(|e| e.to_string())?;
        Self::parse(&raw, path)
    }

    /// 扫描目录下的 `qoder*.json`。
    ///
    /// 返回 `(成功解析的凭证, 失败描述)`。与 [`crate::auth::load_dir`] 的差别是
    /// 那个**静默跳过**失败项，这里把失败**报出来** —— Qoder 凭证可能是用户
    /// 手写的，格式错误必须可见。
    pub fn load_dir(dir: &str) -> (Vec<Self>, Vec<String>) {
        let mut paths: Vec<String> = Vec::new();
        match std::fs::read_dir(dir) {
            Ok(entries) => {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name.starts_with("qoder") && name.ends_with(".json") {
                        paths.push(e.path().to_string_lossy().to_string());
                    }
                }
            }
            Err(_) => return (Vec::new(), Vec::new()),
        }
        paths.sort();

        let mut creds = Vec::new();
        let mut failed = Vec::new();
        for p in paths {
            match Self::load_file(&p) {
                Ok(c) => creds.push(c),
                Err(e) => failed.push(format!("{}: {e}", base_name(&p))),
            }
        }
        (creds, failed)
    }

    /// 原子写回凭证文件（保持**嵌套形**）。
    ///
    /// ★ 保留策略是**从磁盘读回再合并**，而不是只用内存字段重建：
    ///
    /// - 宿主写 `account.no_route`（账号页「停止接流量」），网关写刷新结果。
    ///   Qoder 刷新很频繁，整体覆盖会在几分钟内**悄悄抹掉**用户的禁用标记
    ///   —— 账号重新接流量，而用户以为它还停着。
    /// - 从磁盘读回还顺带保住了宿主后来加的未知字段。
    pub fn save_atomic(&self) -> Result<(), String> {
        if self.file_path.is_empty() {
            return Err("凭证没有来源路径，无法写回".to_string());
        }

        let mut doc: Map<String, Value> = std::fs::read(&self.file_path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();

        let mut auth = Map::new();
        auth.insert("accessToken".to_string(), Value::String(self.dt.clone()));
        auth.insert(
            "refreshToken".to_string(),
            Value::String(self.drt.clone()),
        );
        auth.insert(
            "expiresAt".to_string(),
            Value::Number(self.dt_expires_at.into()),
        );
        auth.insert(
            "domain".to_string(),
            Value::String(self.region.domain().to_string()),
        );
        doc.insert("auth".to_string(), Value::Object(auth));

        let mut account = doc
            .get("account")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        account.insert("uid".to_string(), Value::String(self.uid.clone()));
        account.insert(
            "nickname".to_string(),
            Value::String(self.nickname.clone()),
        );
        doc.insert("account".to_string(), Value::Object(account));

        // 指纹**必须落盘**：不写的话每次重载都会生成新指纹。
        if !self.machine_id.is_empty()
            || !self.machine_token.is_empty()
            || !self.machine_type.is_empty()
        {
            let mut machine = doc
                .get("machine")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            if !self.machine_id.is_empty() {
                machine.insert("id".to_string(), Value::String(self.machine_id.clone()));
            }
            if !self.machine_token.is_empty() {
                machine.insert(
                    "token".to_string(),
                    Value::String(self.machine_token.clone()),
                );
            }
            if !self.machine_type.is_empty() {
                machine.insert(
                    "type".to_string(),
                    Value::String(self.machine_type.clone()),
                );
            }
            doc.insert("machine".to_string(), Value::Object(machine));
        }

        let out = serde_json::to_vec_pretty(&Value::Object(doc))
            .map_err(|e| format!("序列化凭证失败: {e}"))?;
        let tmp = format!("{}.tmp", self.file_path);
        std::fs::write(&tmp, &out).map_err(|e| format!("写入临时文件失败: {e}"))?;
        if let Err(e) = std::fs::rename(&tmp, &self.file_path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("替换凭证文件失败: {e}"));
        }
        Ok(())
    }

    /// 补齐机器指纹，返回是否发生了变化。
    ///
    /// `machine_type` 固定 `"5"`（参考实现固定用 5，对应桌面客户端类型，
    /// 不要臆造其它值）。
    pub fn ensure_fingerprint(&mut self) -> bool {
        let mut changed = false;
        if self.machine_id.is_empty() {
            self.machine_id = uuid4();
            changed = true;
        }
        if self.machine_token.is_empty() {
            self.machine_token = hex_short(32);
            changed = true;
        }
        if self.machine_type.is_empty() {
            self.machine_type = "5".to_string();
            changed = true;
        }
        changed
    }

    /// `dt` 是否需要在 `within` 内刷新。
    pub fn needs_refresh(&self, within: std::time::Duration) -> bool {
        if self.dt.is_empty() {
            return true;
        }
        if self.dt_expires_at <= 0 {
            return true;
        }
        let now = chrono::Utc::now().timestamp();
        now + within.as_secs() as i64 >= self.dt_expires_at
    }

    /// 转成账号池用的 [`crate::auth::Auth`]。
    ///
    /// ★ `no_route` **必须传下去**：池的 `healthy()` 会因它排除该账号；
    /// 漏掉的话界面上的「停止接流量」开关完全不生效。
    /// ★ 机器指纹**刻意不放进 `Auth`** —— `Auth` 不承载产品专属字段，
    /// 产品上游自己重新读凭证文件（见 `dispatch::cred_of`）。
    pub fn to_auth(&self) -> crate::auth::Auth {
        crate::auth::Auth {
            access_token: self.dt.clone(),
            refresh_token: self.drt.clone(),
            expires_at: self.dt_expires_at,
            domain: self.region.domain().to_string(),
            uid: self.uid.clone(),
            nickname: self.nickname.clone(),
            product: crate::auth::PRODUCT_QODER.to_string(),
            no_route: self.no_route,
            file_path: self.file_path.clone(),
            ..Default::default()
        }
    }
}

/// `filepath.Base` 的等价物（同时认 `/` 与 `\`）。
pub(crate) fn base_name(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rsplit(['/', '\\']).next() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => path.to_string(),
    }
}

/// 取第一个 `TrimSpace` 后非空的**原值**。
pub(crate) fn first_non_empty(vals: &[&str]) -> String {
    for v in vals {
        if !v.trim().is_empty() {
            return (*v).to_string();
        }
    }
    String::new()
}

/// 取第一个非 0 的值。
pub(crate) fn first_non_zero(vals: &[i64]) -> i64 {
    for v in vals {
        if *v != 0 {
            return *v;
        }
    }
    0
}

/// UUID v4。
///
/// ★ 算法与参考实现逐字一致，**不要"顺手优化"** —— 签名对字节敏感。
pub(crate) fn uuid4() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let mut s = String::with_capacity(36);
    for (i, byte) in b.iter().enumerate() {
        if i == 4 || i == 6 || i == 8 || i == 10 {
            s.push('-');
        }
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// `n` 个 ASCII 十六进制字符（★ **不是 n 个随机字节**）。
///
/// 签名层用它生成临时 AES 密钥（16 个 ASCII 字符），改错会导致上游
/// 回 `101 Signature invalid`。
pub(crate) fn hex_short(n: usize) -> String {
    use rand::Rng;
    let mut b = vec![0u8; n.div_ceil(2)];
    rand::thread_rng().fill(&mut b[..]);
    let s = hex::encode(b);
    s[..n.min(s.len())].to_string()
}

/// 签名层用的 requestId。
pub(crate) fn new_uuid() -> String {
    uuid4()
}

/// 排序 + 紧凑 JSON（签名对字节敏感，不能用普通序列化）。
pub(crate) fn json_sorted_compact(m: &BTreeMap<String, String>) -> Vec<u8> {
    let mut out = String::with_capacity(256);
    out.push('{');
    for (i, (k, v)) in m.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::to_string(k).unwrap_or_default());
        out.push(':');
        out.push_str(&serde_json::to_string(v).unwrap_or_default());
    }
    out.push('}');
    out.into_bytes()
}

/// Go `truncate(s, n)`：超长时截断并加省略号（按**字节**）。
pub(crate) fn truncate_str(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested_json() -> String {
        r#"{
          "auth": {
            "accessToken": "dt-placeholder-token",
            "refreshToken": "drt-placeholder-token",
            "expiresAt": 1800000000,
            "domain": "qoder.com.cn"
          },
          "account": { "uid": "00000000-0000-4000-8000-000000000001", "nickname": "占位昵称", "no_route": true },
          "machine": { "id": "11111111-1111-4111-8111-111111111111", "token": "deadbeefdeadbeefdeadbeefdeadbeef", "type": "5" },
          "unknown_future_field": { "keep": "me" }
        }"#
        .to_string()
    }

    #[test]
    fn parse_nested_form_keeps_everything() {
        let c = QoderCred::parse(nested_json().as_bytes(), "C:\\tmp\\qoder-abc.json").unwrap();
        assert_eq!(c.dt, "dt-placeholder-token");
        assert_eq!(c.drt, "drt-placeholder-token");
        assert_eq!(c.dt_expires_at, 1800000000);
        assert_eq!(c.uid, "00000000-0000-4000-8000-000000000001");
        assert_eq!(c.nickname, "占位昵称");
        assert!(c.no_route, "account.no_route 必须读回来");
        assert_eq!(c.region, QoderRegion::CN);
        // ★ machine 段必须读回来，否则指纹每次都变。
        assert_eq!(c.machine_id, "11111111-1111-4111-8111-111111111111");
        assert_eq!(c.machine_token, "deadbeefdeadbeefdeadbeefdeadbeef");
        assert_eq!(c.machine_type, "5");
        assert_eq!(c.file_path, "C:\\tmp\\qoder-abc.json");
    }

    #[test]
    fn parse_flat_form_and_uid_from_filename() {
        let raw = r#"{"token":"dt-flat","refresh_token":"drt-flat","expires_at_unix":1700000000,"nickname":"扁平"}"#;
        let c = QoderCred::parse(raw.as_bytes(), "/auths/qoder-42.json").unwrap();
        assert_eq!(c.dt, "dt-flat");
        assert_eq!(c.drt, "drt-flat");
        assert_eq!(c.dt_expires_at, 1700000000);
        assert_eq!(c.uid, "42", "UID 缺失时从文件名兜底");
        assert_eq!(c.region, QoderRegion::Unknown, "没有 domain ⇒ 未知，不猜");
        assert!(!c.no_route);
    }

    #[test]
    fn parse_flat_form_accepts_camel_case_aliases() {
        let raw = r#"{"accessToken":"dt-camel","refreshToken":"drt-camel","expiresAt":1700000001,"user_id":"u-9","domain":"qoder.sh"}"#;
        let c = QoderCred::parse(raw.as_bytes(), "qoderwork-u-9.json").unwrap();
        assert_eq!(c.dt, "dt-camel");
        assert_eq!(c.drt, "drt-camel");
        assert_eq!(c.dt_expires_at, 1700000001);
        assert_eq!(c.uid, "u-9", "user_id 优先于文件名兜底");
        assert_eq!(c.region, QoderRegion::Intl);
    }

    #[test]
    fn parse_rejects_empty_and_bad_and_tokenless() {
        assert!(QoderCred::parse(b"", "x.json").unwrap_err().contains("凭证文件为空"));
        assert!(QoderCred::parse(b"{oops", "x.json")
            .unwrap_err()
            .contains("凭证不是合法 JSON"));
        let e = QoderCred::parse(br#"{"uid":"u1"}"#, "x.json").unwrap_err();
        assert!(e.contains("既没有 accessToken 也没有 refreshToken"), "{e}");
    }

    #[test]
    fn ensure_fingerprint_is_idempotent_and_stable() {
        let mut c = QoderCred::default();
        assert!(c.ensure_fingerprint(), "首次必须补齐");
        assert!(!c.ensure_fingerprint(), "第二次不得再改");
        assert_eq!(c.machine_type, "5");
        assert_eq!(c.machine_id.len(), 36);
        assert_eq!(c.machine_token.len(), 32);
        let id = c.machine_id.clone();
        let tok = c.machine_token.clone();
        assert!(!c.ensure_fingerprint());
        assert_eq!(c.machine_id, id);
        assert_eq!(c.machine_token, tok);
    }

    #[test]
    fn save_atomic_preserves_no_route_and_unknown_fields() {
        let dir = std::env::temp_dir().join(format!("qoder-cred-test-{}", uuid4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("qoder-save.json");
        let path_s = path.to_string_lossy().to_string();

        // 宿主先写：no_route + 未知字段 + 没有 machine 段。
        let host = r#"{
  "account": { "uid": "u-save", "nickname": "旧昵称", "no_route": true, "host_only": 7 },
  "auth": { "accessToken": "old", "refreshToken": "old-r" }
}"#;
        std::fs::write(&path, host).unwrap();

        let mut c = QoderCred::parse(host.as_bytes(), &path_s).unwrap();
        assert!(c.no_route);
        c.dt = "dt-new".to_string();
        c.drt = "drt-new".to_string();
        c.dt_expires_at = 1800000000;
        c.region = QoderRegion::CN;
        c.ensure_fingerprint();
        c.save_atomic().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let doc: Map<String, Value> = serde_json::from_str(&text).unwrap();
        let account = doc["account"].as_object().unwrap();
        assert_eq!(account["no_route"], Value::Bool(true), "禁用标记被抹掉了");
        assert_eq!(account["host_only"], Value::Number(7.into()), "未知字段丢了");
        assert_eq!(doc["auth"]["accessToken"], Value::String("dt-new".into()));
        assert_eq!(doc["machine"]["type"], Value::String("5".into()));

        // 再读回来，指纹必须与刚写的一致（回归：指纹跨加载稳定）。
        let again = QoderCred::load_file(&path_s).unwrap();
        assert_eq!(again.machine_id, c.machine_id);
        assert_eq!(again.machine_token, c.machine_token);
        assert_eq!(again.machine_type, c.machine_type);
        assert!(again.no_route);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_atomic_refuses_without_path() {
        let c = QoderCred::default();
        assert!(c.save_atomic().unwrap_err().contains("没有来源路径"));
    }

    #[test]
    fn load_dir_reports_failures_instead_of_skipping() {
        let dir = std::env::temp_dir().join(format!("qoder-dir-test-{}", uuid4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("qoder-ok.json"),
            r#"{"token":"dt","refresh_token":"drt"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("qoder-bad.json"), "{not json").unwrap();
        std::fs::write(dir.join("other.json"), "{}").unwrap();

        let (creds, failed) = QoderCred::load_dir(&dir.to_string_lossy());
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].uid, "ok", "文件名兜底 UID");
        assert_eq!(failed.len(), 1);
        assert!(failed[0].starts_with("qoder-bad.json: "), "{failed:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_sorted_compact_is_sorted_and_tight() {
        let mut m = BTreeMap::new();
        m.insert("b".to_string(), "2".to_string());
        m.insert("a".to_string(), "1".to_string());
        m.insert("quote".to_string(), "he said \"hi\"".to_string());
        let got = String::from_utf8(json_sorted_compact(&m)).unwrap();
        assert_eq!(got, r#"{"a":"1","b":"2","quote":"he said \"hi\""}"#);
    }

    #[test]
    fn hex_short_is_ascii_chars_not_bytes() {
        let h = hex_short(16);
        assert_eq!(h.len(), 16, "16 个 ASCII 字符（不是 16 个随机字节）");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(hex_short(32).len(), 32);
        assert_eq!(hex_short(3).len(), 3);
    }

    #[test]
    fn to_auth_carries_no_route_and_product() {
        let mut c = QoderCred::parse(nested_json().as_bytes(), "C:\\tmp\\qoder-abc.json").unwrap();
        c.ensure_fingerprint();
        let a = c.to_auth();
        assert_eq!(a.product, crate::auth::PRODUCT_QODER);
        assert!(a.is_qoder());
        assert!(a.no_route);
        assert_eq!(a.uid, c.uid);
        assert_eq!(a.domain, "qoder.com.cn");
        assert_eq!(a.file_path, "C:\\tmp\\qoder-abc.json");
    }

    #[test]
    fn needs_refresh_boundaries() {
        let mut c = QoderCred::default();
        assert!(c.needs_refresh(std::time::Duration::from_secs(7200)), "没有 dt");
        c.dt = "dt".to_string();
        assert!(c.needs_refresh(std::time::Duration::from_secs(7200)), "到期时间未知");
        c.dt_expires_at = chrono::Utc::now().timestamp() + 3600;
        assert!(c.needs_refresh(std::time::Duration::from_secs(7200)), "1 小时后到期，2 小时窗口内");
        assert!(!c.needs_refresh(std::time::Duration::from_secs(60)), "60 秒窗口内不该刷");
    }

    #[test]
    fn base_name_handles_both_separators() {
        assert_eq!(base_name("C:\\a\\b\\qoder-1.json"), "qoder-1.json");
        assert_eq!(base_name("/a/b/qoder-2.json"), "qoder-2.json");
        assert_eq!(base_name("qoder-3.json"), "qoder-3.json");
    }
}
