//! zcode_credstore.rs 读 ZCode 客户端 `credentials.json` 里的**账号信息**。
//!
//! ## 为什么需要这个
//!
//! 所有者的反馈（两轮）：
//!
//!   1. 「ZCode 的名字获取的也不对，应该能从接口获取的」
//!   2. 「你那玩意而是套餐名，我现在登录的账号明明有名字」
//!
//! 第一轮我拿了客户端配置的 `name`（"BigModel - Coding Plan"）——
//! 那确实是**套餐名**。第二轮我穷举了 156 种接口组合、扫了本地存储、
//! 翻了客户端日志，**接口都拿不到账号名**（JWT 已过期，auth 系全 401）。
//!
//! 而客户端界面上明明显示着名字。它就在
//! `~/.zcode/v2/credentials.json` 的 `oauth:bigmodel:user_info` 里，
//! 只是被 `enc:v1:` 加密了：
//!
//!   {"id":"19331730795565300","username":"wish","displayName":"wish","avatarUrl":"..."}
//!
//! ## 解密方案（**逐字**从客户端 app.asar 提取，不是猜的）
//!
//! 客户端源码（`D:\APP\ZCode\resources\app.asar` 内）：
//!
//! ```js
//! const PREFIX = "enc:v1:", ALGO = "aes-256-gcm", IV_LEN = 12, TAG_LEN = 16;
//! const ENV_SECRET = "ZCODE_CREDENTIAL_SECRET";
//! const deriveCipherKey = (s) => createHash("sha256").update(s).digest();
//! const defaultCredentialSecret = (env) =>
//!   env[ENV_SECRET] ??
//!   `zcode-credential-fallback:${platform()}:${homedir()}:${userInfo().username}`;
//!
//! decrypt(o) {
//!   let [iv, tag, ct] = o.slice(PREFIX.length).split(".");
//!   if (iv.length !== 12) throw "IV 长度非法";
//!   if (tag.length !== 16) throw "AuthTag 长度非法";
//!   let d = createDecipheriv(ALGO, key, iv);
//!   d.setAuthTag(tag);
//!   return Buffer.concat([d.update(ct), d.final()]).toString("utf-8");
//! }
//! ```
//!
//! ⚠ 分段顺序是 **`iv.tag.ciphertext`**，**不是**常见的
//! `nonce.ciphertext.tag`。我第一版按惯例猜，报 `nonce 长度 16（应为 12）`。
//! 教训：跨实现复刻**必须读源码**，不能按惯例猜。
//!
//! ## 边界说明
//!
//! 这是**用户自己机器上、用户自己的**凭证，用户明确要求"直接导入"。
//! 客户端加密只是为了"不裸奔"，密钥完全由公开信息派生
//!（platform / homedir / 系统用户名）—— 它挡的是别的用户，不是本机用户。
//!
//! 本模块**只读**，且只取**身份字段**（用户名等），不改动该文件。

use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// 加密值前缀。
const PREFIX: &str = "enc:v1:";
/// IV 长度（**第一段**）。
const IV_LEN: usize = 12;
/// AuthTag 长度（**第二段**）。
const TAG_LEN: usize = 16;
/// 可覆盖密钥的环境变量名（与客户端一致）。
const ENV_SECRET: &str = "ZCODE_CREDENTIAL_SECRET";

/// 从 credentials.json 里读出的账号信息。
#[derive(Debug, Clone, Default)]
pub struct ZcodeIdentity {
    /// 用户名（如 `wish`）。
    pub username: String,
    /// 展示名（通常与 username 相同）。
    pub display_name: String,
    /// 账号 ID（数字串）。
    pub id: String,
    /// 头像 URL。
    pub avatar_url: String,
    /// 当前活跃服务商（`bigmodel` / `zai`）。
    pub active_provider: String,
}

impl ZcodeIdentity {
    /// 最适合给用户看的名字。
    ///
    /// 优先级：displayName → username → id。
    /// 全都空时返回空串（调用方据此回退到别的名字源）。
    pub fn best_name(&self) -> &str {
        for s in [&self.display_name, &self.username, &self.id] {
            if !s.trim().is_empty() {
                return s.trim();
            }
        }
        ""
    }
}

/// credentials.json 的位置。
pub fn credentials_file() -> PathBuf {
    // 与 zcode_scan 同样的基准：`~/.zcode`
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    match home {
        Some(h) => h.join(".zcode").join("v2").join("credentials.json"),
        None => PathBuf::from("credentials.json"),
    }
}

/// 客户端默认密钥的派生输入。
///
/// `zcode-credential-fallback:${platform}:${homedir}:${username}`
///
/// 全部是**公开的机器信息** —— 这正是"它挡别的用户、不挡本机用户"的体现。
pub fn default_secret() -> String {
    if let Ok(s) = std::env::var(ENV_SECRET) {
        if !s.is_empty() {
            return s;
        }
    }
    let platform = if cfg!(windows) {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    let home = home_dir().unwrap_or_else(|| "unknown".to_string());
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_string());
    format!("zcode-credential-fallback:{platform}:{home}:{user}")
}

fn home_dir() -> Option<String> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|s| s.to_string_lossy().into_owned())
}

/// 由 seed 派生 AES-256 密钥（sha256）。
pub fn derive_key(secret: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(secret.as_bytes());
    h.finalize().into()
}

/// 解密一个 `enc:v1:` 值；不是该格式时原样返回（客户端约定）。
pub fn decrypt_value(raw: &str, key: &[u8; 32]) -> Result<String, String> {
    let Some(body) = raw.strip_prefix(PREFIX) else {
        // 客户端就是这么做的：非密文直接返回（便于手工写入明文调试）
        return Ok(raw.to_string());
    };
    let parts: Vec<&str> = body.split('.').collect();
    if parts.len() != 3 {
        return Err(format!("密文格式非法（分段数 {}，应为 3）", parts.len()));
    }
    let (iv_b64, tag_b64, ct_b64) = (parts[0], parts[1], parts[2]);
    if iv_b64.is_empty() || tag_b64.is_empty() || ct_b64.is_empty() {
        return Err("密文格式非法（有空段）".into());
    }

    let eng = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let iv = eng.decode(iv_b64).map_err(|e| format!("IV 不是合法 base64url: {e}"))?;
    let tag = eng.decode(tag_b64).map_err(|e| format!("AuthTag 不是合法 base64url: {e}"))?;
    let ct = eng.decode(ct_b64).map_err(|e| format!("密文不是合法 base64url: {e}"))?;

    if iv.len() != IV_LEN {
        return Err(format!("IV 长度非法（{}，应为 {IV_LEN}）", iv.len()));
    }
    if tag.len() != TAG_LEN {
        return Err(format!("AuthTag 长度非法（{}，应为 {TAG_LEN}）", tag.len()));
    }

    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("密钥长度非法: {e}"))?;
    // aes-gcm 的 `decrypt` 期望 `ciphertext || tag`（tag 在后），
    // 而客户端把 tag 单独放在第二段 —— 故这里拼一下。
    let mut combined = ct;
    combined.extend_from_slice(&tag);

    let plain = cipher
        .decrypt(Nonce::from_slice(&iv), combined.as_ref())
        .map_err(|_| "密钥不匹配或密文已损坏".to_string())?;
    String::from_utf8(plain).map_err(|e| format!("解密结果不是 UTF-8: {e}"))
}

/// 从已解析的 credentials.json 里抽出账号信息。
///
/// 找不到对应键时返回 `None`（调用方回退），而不是报错 ——
/// 用户可能只登录了部分服务商。
pub fn identity_from_doc(doc: &Value, key: &[u8; 32]) -> Option<ZcodeIdentity> {
    let obj = doc.as_object()?;

    // user_info 里有 username / displayName；键名按服务商变化，
    // 故按后缀匹配而不是写死 `oauth:bigmodel:user_info`。
    let mut ident = ZcodeIdentity::default();
    for (k, v) in obj {
        if !k.ends_with(":user_info") {
            continue;
        }
        let Some(raw) = v.as_str() else { continue };
        let Ok(plain) = decrypt_value(raw, key) else { continue };
        let Ok(info) = serde_json::from_str::<Value>(&plain) else { continue };
        ident.username = str_of(&info, "username");
        ident.display_name = str_of(&info, "displayName");
        ident.id = str_of(&info, "id");
        ident.avatar_url = str_of(&info, "avatarUrl");
        break;
    }

    // 活跃服务商（明文，不解密也行，但统一走 decrypt 更稳）
    if let Some(raw) = obj.get("oauth:active_provider").and_then(Value::as_str) {
        if let Ok(p) = decrypt_value(raw, key) {
            ident.active_provider = p.trim().to_string();
        }
    }

    if ident.best_name().is_empty() {
        None
    } else {
        Some(ident)
    }
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// 读本机 ZCode 客户端凭证，返回账号信息。
///
/// 任何一步失败都返回 `None`：这只是"名字更好看"的增强，
/// 拿不到时调用方回退到别的名字源，**不该**因此让整个页面报错。
pub fn read_identity() -> Option<ZcodeIdentity> {
    read_identity_from(&credentials_file())
}

/// 可测试入口：从指定文件读。
pub fn read_identity_from(path: &Path) -> Option<ZcodeIdentity> {
    let raw = std::fs::read_to_string(path).ok()?;
    let doc: Value = serde_json::from_str(&raw).ok()?;
    let key = derive_key(&default_secret());
    identity_from_doc(&doc, &key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 用**客户端同款**算法加密一个值（供测试造夹具）。
    ///
    /// 刻意独立实现一遍（而不是调 decrypt 的逆）—— 若两边共用同一段
    /// 有 bug 的代码，测试就测不出来了。
    fn client_encrypt(plain: &str, secret: &str) -> String {
        use aes_gcm::aead::OsRng;
        use aes_gcm::aead::rand_core::RngCore;

        let key = derive_key(secret);
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();

        // 12 字节 IV + 16 字节 tag，分段顺序 iv.tag.ct（与客户端一致）
        let mut iv = [0u8; IV_LEN];
        OsRng.fill_bytes(&mut iv);

        let ct_and_tag = cipher.encrypt(Nonce::from_slice(&iv), plain.as_bytes()).unwrap();
        let (ct, tag) = ct_and_tag.split_at(ct_and_tag.len() - TAG_LEN);

        let eng = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{PREFIX}{}.{}.{}",
            eng.encode(iv),
            eng.encode(tag),
            eng.encode(ct)
        )
    }

    // 与客户端同款算法加解密能往返 —— 证明分段顺序理解正确。
    //
    // ⚠ 这条测试的价值在于**顺序**：我第一版按 `nonce.ct.tag` 的惯例猜，
    // 结果全解不开。跨实现复刻必须读源码。
    #[test]
    fn roundtrip_matches_client_format() {
        let secret = "test-secret";
        let key = derive_key(secret);
        let plain = r#"{"username":"wish","displayName":"wish"}"#;
        let enc = client_encrypt(plain, secret);
        assert!(enc.starts_with("enc:v1:"));
        assert_eq!(enc.matches('.').count(), 2, "应是三段");
        let got = decrypt_value(&enc, &key).unwrap();
        assert_eq!(got, plain);
    }

    // 非 enc:v1: 前缀的值原样返回（客户端约定，便于手工写明文调试）
    #[test]
    fn plaintext_passes_through() {
        let key = derive_key("whatever");
        assert_eq!(decrypt_value("plain-value", &key).unwrap(), "plain-value");
        assert_eq!(decrypt_value("", &key).unwrap(), "");
    }

    // 密钥不对时要**明确报错**，不能返回空串。
    //
    // 返回空串会让调用方把"解密失败"误当成"这个账号没有名字"，
    // 于是静默回退 —— 用户永远不知道是密钥不匹配。
    #[test]
    fn wrong_key_is_an_error_not_empty() {
        let enc = client_encrypt("secret-data", "the-right-secret");
        let wrong = derive_key("the-wrong-secret");
        match decrypt_value(&enc, &wrong) {
            Ok(v) => panic!("密钥不匹配应报错，实际返回 {v:?}"),
            Err(e) => assert!(
                e.contains("密钥不匹配") || e.contains("损坏"),
                "错误信息应说明原因，实际 {e:?}"
            ),
        }
    }

    // 分段数不对要报错（明确告知，而不是静默失败）
    #[test]
    fn malformed_segments_error() {
        let key = derive_key("k");
        for bad in ["enc:v1:only-one", "enc:v1:a.b", "enc:v1:a.b.c.d", "enc:v1:.."] {
            assert!(decrypt_value(bad, &key).is_err(), "应报错: {bad}");
        }
    }

    // 真实形状（本机实测值手工脱敏后）→ 能解析出名字。
    #[test]
    fn parses_real_credentials_shape() {
        let secret = "fixture-secret";
        let key = derive_key(secret);
        let info = json!({
            "id": "19331730795565300",
            "username": "wish",
            "displayName": "wish",
            "avatarUrl": "https://example.invalid/a.png",
            "rawProfile": {"zcodeProfileMigrationRetryAfter": 1786351398839i64}
        });
        let doc = json!({
            "oauth:bigmodel:access_token": client_encrypt("eyJhbGciOiJI.fake.token", secret),
            "zcodejwttoken": client_encrypt("eyJhbGciOiJI.fake.jwt", secret),
            "oauth:bigmodel:user_info": client_encrypt(&info.to_string(), secret),
            "oauth:active_provider": client_encrypt("bigmodel", secret),
        });

        let ident = identity_from_doc(&doc, &key).expect("应解析出身份");
        assert_eq!(ident.username, "wish");
        assert_eq!(ident.display_name, "wish");
        assert_eq!(ident.id, "19331730795565300");
        assert_eq!(ident.active_provider, "bigmodel");
        assert_eq!(ident.best_name(), "wish");
    }

    // 只有 username、没有 displayName 时也要能给出名字
    #[test]
    fn best_name_falls_back_through_fields() {
        let secret = "s";
        let key = derive_key(secret);
        let doc = json!({
            "oauth:zai:user_info": client_encrypt(r#"{"username":"only-user"}"#, secret)
        });
        let ident = identity_from_doc(&doc, &key).unwrap();
        assert_eq!(ident.best_name(), "only-user");

        // displayName 优先于 username
        let doc2 = json!({
            "oauth:zai:user_info": client_encrypt(r#"{"username":"u","displayName":"显示名"}"#, secret)
        });
        let i2 = identity_from_doc(&doc2, &key).unwrap();
        assert_eq!(i2.best_name(), "显示名");
    }

    // 解不开时返回 None（而不是造一个空名字）—— 让调用方回退，
    // 而不是在界面上显示一个空白。
    #[test]
    fn undecryptable_yields_none() {
        let key = derive_key("right");
        let doc = json!({
            "oauth:bigmodel:user_info": client_encrypt(r#"{"username":"x"}"#, "other-secret")
        });
        assert!(identity_from_doc(&doc, &key).is_none());
    }

    // 完全没有 user_info 时返回 None（用户可能只登录了一半）
    #[test]
    fn missing_user_info_yields_none() {
        let key = derive_key("k");
        assert!(identity_from_doc(&json!({}), &key).is_none());
        assert!(identity_from_doc(&json!({"other": "x"}), &key).is_none());
        // 值不是字符串
        assert!(identity_from_doc(&json!({"oauth:x:user_info": 123}), &key).is_none());
    }

    // 密钥派生与客户端一致（sha256）。
    #[test]
    fn derive_key_is_sha256() {
        let k = derive_key("abc");
        // sha256("abc") 的已知值
        let expect = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        assert_eq!(k, expect, "derive_key 必须是 sha256（与客户端一致）");
    }

    // 环境变量能覆盖密钥（客户端支持 ZCODE_CREDENTIAL_SECRET）
    #[test]
    fn env_secret_overrides_default() {
        // 这个测试会改环境变量，用单独的子进程语义更安全；
        // 这里只验证"设了就一定用到"这一点（不并发跑其它用例时安全）。
        let prev = std::env::var(ENV_SECRET).ok();
        std::env::set_var(ENV_SECRET, "from-env-12345");
        assert_eq!(default_secret(), "from-env-12345");
        match prev {
            Some(v) => std::env::set_var(ENV_SECRET, v),
            None => std::env::remove_var(ENV_SECRET),
        }
    }
}
