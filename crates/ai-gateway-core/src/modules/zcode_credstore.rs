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
//!   {"id":"12345678901234567","username":"wish","displayName":"wish","avatarUrl":"..."}
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
    /// 展示名（实测上游叫 `name`，如 `旅行者5800`）。
    pub display_name: String,
    /// 账号 ID（数字串或 UUID）。
    pub id: String,
    /// 头像 URL。
    pub avatar_url: String,
    /// 当前活跃服务商（`bigmodel` / `zai`）。
    pub active_provider: String,
    /// 邮箱；**手机号登录时是 `{手机号}@phone.local`**。
    ///
    /// 见 `phone_from_email()` 与 `best_name()` 的说明。
    pub email: String,
    /// 手机号（从 `email` 解析得出；不是手机号登录时为空）。
    ///
    /// 在 `identity_from_doc` 解析时就填好，而不是每次现算 ——
    /// 这样 `best_name()` 可以照旧返回 `&str`（既有调用点依赖该签名）。
    pub phone: String,
}

impl ZcodeIdentity {
    /// 最适合给用户看的名字。
    ///
    /// # 优先级（2026-09-21 按所有者要求调整）
    ///
    ///	1. `display_name`（上游字段 `name`，如 `旅行者5800`）
    ///	2. `username`
    ///	3. **手机号**（从 email 里提取）
    ///	4. `id`
    ///
    /// 所有者原话：
    ///
    /// > 「我记得接口返回的有，名字跟手机号，**没名字就显示手机号**」
    ///
    /// ⚠ 手机号排在 `id` **之前**：`id` 是一串 UUID/长数字，对用户没有
    /// 任何辨识意义；手机号至少是他自己的号。两者都没有时才回退到 id。
    pub fn best_name(&self) -> &str {
        for s in [&self.display_name, &self.username, &self.phone, &self.id] {
            if !s.trim().is_empty() {
                return s.trim();
            }
        }
        ""
    }

    /// 展示名（拥有所有权的版本），供需要 `String` 的调用方。
    pub fn best_name_owned(&self) -> String {
        self.best_name().to_string()
    }
}

/// 从 `{手机号}@phone.local` 形态的 email 里提取手机号。
///
/// # 为什么要单独处理
///
/// 上游对**手机号登录**的账号构造一个假邮箱：
///
/// ```text
/// 13900000000@phone.local
/// ```
///
/// `phone.local` 是保留域名（不可解析），它不是真邮箱。直接显示
/// 「13900000000@phone.local」对用户是噪音；而只显示号码才是他认得的。
///
/// # 判据
///
/// 域名是 `phone.local`，且 `@` 前是**纯数字**（长度 >= 6）。
/// 任一不满足就返回空串 —— 那说明它是真邮箱（如 `a@b.com`），
/// 此时**不该**把它当手机号显示。
fn phone_from_email(email: &str) -> String {
    let e = email.trim();
    let Some((local, domain)) = e.split_once('@') else {
        return String::new();
    };
    if !domain.eq_ignore_ascii_case("phone.local") {
        return String::new();
    }
    let local = local.trim();
    if local.len() < 6 || !local.chars().all(|c| c.is_ascii_digit()) {
        return String::new();
    }
    local.to_string()
}

/// 按顺序取第一个非空字符串字段（上游不同服务商/版本键名不一致）。
fn first_of(v: &Value, keys: &[&str]) -> String {
    for k in keys {
        let s = str_of(v, k);
        if !s.is_empty() {
            return s;
        }
    }
    String::new()
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

    // user_info 里有账号名与头像；键名按服务商变化，
    // 故按后缀匹配而不是写死 `oauth:bigmodel:user_info`。
    //
    // # ⚠⚠ 2026-09-21 修正：字段名此前**全读错了**
    //
    // 所有者原话：
    //
    // > 「而且 zcode 到现在都没获取到正确的名字，也要修复，我记得接口返回的
    // >   有，名字跟手机号，没名字就显示手机号」
    //
    // 实测解密官方 `oauth:zai:user_info`（本机真实值）：
    //
    // ```json
    // {"user_id":"…","email":"13900000000@phone.local",
    //  "avatar":"https://chat.z.ai/user.png","name":"旅行者5800"}
    // ```
    //
    // 而旧代码读的是 `username` / `displayName` / `id` / `avatarUrl` ——
    // **四个字段名全部不存在** ⇒ 解析结果恒为空 ⇒ 界面上永远没有名字。
    //
    // 教训：这段代码是照着**参考实现的示例**写的（它的样例用
    // `username`/`displayName`），而从没对着**真实的解密结果**核对过。
    // 与 `CAPTURED-SPEC.md` 那条教训同源 —— 二手示例不能当规格。
    //
    // # 现在按**优先级**读多个候选名
    //
    // 上游不同服务商/版本的键名不一致，故每个字段都给出候选：
    //
    //	名字   name → username → displayName → nickName
    //	标识   user_id → id → sub
    //	头像   avatar → avatarUrl
    //	邮箱   email（手机号登录时形如 `{手机号}@phone.local`）
    let mut ident = ZcodeIdentity::default();
    for (k, v) in obj {
        if !k.ends_with(":user_info") {
            continue;
        }
        let Some(raw) = v.as_str() else { continue };
        let Ok(plain) = decrypt_value(raw, key) else { continue };
        let Ok(info) = serde_json::from_str::<Value>(&plain) else { continue };
        // 名字：实测是 `name`（参考实现的 `username`/`displayName` 不存在，
        // 但保留为候选 —— 别的服务商可能用它们）。
        ident.display_name = first_of(&info, &["name", "displayName", "nickName", "username"]);
        ident.username = first_of(&info, &["username", "name", "nickName"]);
        // 账号标识：实测是 `user_id`。
        ident.id = first_of(&info, &["user_id", "id", "sub"]);
        // 头像：实测是 `avatar`。
        ident.avatar_url = first_of(&info, &["avatar", "avatarUrl"]);
        // 邮箱/手机号 —— 手机号登录时上游把它塞在 email 里，
        // 形如 `13900000000@phone.local`（见下面 phone_from_email 的说明）。
        ident.email = first_of(&info, &["email", "phoneNumber", "phone"]);
        // 手机号在**解析时**就提取好，这样 best_name() 能照旧返回 &str。
        ident.phone = phone_from_email(&ident.email);
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
    //
    // ⚠ 本用例用的是**参考实现示例**里的字段名（`username`/`displayName`），
    // 而**不是**上游真实返回的名字 —— 这正是那个 bug 藏了这么久的原因：
    // 测试与实现读了同一份错误的示例，于是"测试通过"掩盖了"线上没名字"。
    // 真实形状见下面 `parses_upstream_real_field_names`。
    #[test]
    fn parses_real_credentials_shape() {
        let secret = "fixture-secret";
        let key = derive_key(secret);
        let info = json!({
            "id": "12345678901234567",
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
        assert_eq!(ident.id, "12345678901234567");
        assert_eq!(ident.active_provider, "bigmodel");
        assert_eq!(ident.best_name(), "wish");
    }

    /// ★ 上游**真实**字段名：`name` / `user_id` / `avatar` / `email`。
    ///
    /// # 为什么必须有这条（2026-09-21 所有者报的缺陷）
    ///
    /// 所有者原话：
    ///
    /// > 「而且 zcode 到现在都没获取到正确的名字，也要修复，我记得接口返回的
    /// >   有，名字跟手机号，没名字就显示手机号」
    ///
    /// 实测解密官方 `oauth:zai:user_info`（本机真实值，此处已脱敏）：
    ///
    /// ```json
    /// {"user_id":"{uuid}","email":"{手机号}@phone.local",
    ///  "avatar":"https://chat.z.ai/user.png","name":"旅行者5800"}
    /// ```
    ///
    /// 而旧实现读的是 `username` / `displayName` / `id` / `avatarUrl` ——
    /// **四个字段名全部不存在** ⇒ 恒为空 ⇒ 界面上永远没有名字。
    ///
    /// 夹具**逐字**照真实形状写，故意**不含** `username`/`displayName`：
    /// 若实现又退回只读那两个字段，这条会红。
    #[test]
    fn parses_upstream_real_field_names() {
        let secret = "fixture-secret";
        let key = derive_key(secret);
        let info = json!({
            "user_id": "00000000-0000-4000-8000-000000000001",
            "email": "13900000000@phone.local",
            "avatar": "https://chat.z.ai/user.png",
            "name": "旅行者5800"
        });
        let doc = json!({
            "oauth:zai:user_info": client_encrypt(&info.to_string(), secret),
            "oauth:active_provider": client_encrypt("zai", secret),
        });

        let ident = identity_from_doc(&doc, &key).expect("应解析出身份");
        assert_eq!(ident.display_name, "旅行者5800", "名字在 `name` 字段里");
        assert_eq!(
            ident.id, "00000000-0000-4000-8000-000000000001",
            "账号标识在 `user_id` 字段里"
        );
        assert_eq!(ident.avatar_url, "https://chat.z.ai/user.png", "头像在 `avatar` 里");
        assert_eq!(ident.email, "13900000000@phone.local");
        assert_eq!(ident.phone, "13900000000", "手机号应从 email 里提取出来");
        assert_eq!(ident.best_name(), "旅行者5800", "有名字就用名字");
    }

    /// 没有名字时**回退到手机号**（所有者明确要求）。
    ///
    /// 所有者原话：
    ///
    /// > 「我记得接口返回的有，名字跟手机号，**没名字就显示手机号**」
    #[test]
    fn falls_back_to_phone_when_no_name() {
        let secret = "fixture-secret";
        let key = derive_key(secret);
        // 刻意只有 email、没有 name
        let info = json!({
            "user_id": "00000000-0000-4000-8000-000000000002",
            "email": "13900000000@phone.local",
            "avatar": "https://chat.z.ai/user.png"
        });
        let doc = json!({
            "oauth:zai:user_info": client_encrypt(&info.to_string(), secret),
        });

        let ident = identity_from_doc(&doc, &key).expect("有手机号也算解析成功");
        assert_eq!(ident.best_name(), "13900000000", "没名字就该显示手机号");
    }

    /// 手机号**优先于 id**（id 对用户没有辨识意义）。
    #[test]
    fn phone_preferred_over_id() {
        let mut ident = ZcodeIdentity {
            id: "00000000-0000-4000-8000-000000000003".into(),
            phone: "13900000000".into(),
            email: "13900000000@phone.local".into(),
            ..Default::default()
        };
        assert_eq!(
            ident.best_name(),
            "13900000000",
            "手机号应优先于 id —— id 是一串 UUID，用户认不出来"
        );
        // 连手机号都没有时才回退 id
        ident.phone.clear();
        ident.email.clear();
        assert_eq!(ident.best_name(), "00000000-0000-4000-8000-000000000003");
    }

    /// `phone_from_email` 只认 `{纯数字}@phone.local`，真邮箱一律返回空。
    ///
    /// # 为什么要严格
    ///
    /// 把 `someone@example.com` 当手机号显示是**编造数据** ——
    /// 用户会以为那是自己的号码。故域名与内容都要校验。
    #[test]
    fn phone_from_email_is_strict() {
        // 认
        assert_eq!(phone_from_email("13900000000@phone.local"), "13900000000");
        assert_eq!(phone_from_email("13900000000@PHONE.LOCAL"), "13900000000", "域名大小写不敏感");
        assert_eq!(phone_from_email(" 13900000000@phone.local "), "13900000000", "两侧空白应忽略");
        // 不认（真邮箱 / 形态不对）
        for bad in [
            "someone@example.com",
            "13900000000@example.com",
            "abc@phone.local",       // 非纯数字
            "12345@phone.local",     // 太短（<6）
            "13900000000",           // 没有 @
            "@phone.local",          // 本地部分为空
            "",
        ] {
            assert_eq!(
                phone_from_email(bad),
                "",
                "不该把 {bad:?} 当成手机号（那是编造数据）"
            );
        }
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
