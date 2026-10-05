//! 读 **Qoder 客户端自己**的登录凭证，供一键导入
//! （Go `internal/qoderclient/clientauth.go` 的移植）。
//!
//! # 为什么做这个（以及它比网页授权好在哪）
//!
//! 网页授权的 `redirect_uri` 是**自定义协议**（`qoder-work-cn://`），本机
//! **没有注册这个协议**（我们不是 Qoder 客户端，不会去注册它）—— 浏览器授权
//! 完成后无处回调。⚠ 但**轮询**端点形状是**对的**：拿会话里的 nonce/verifier 去打
//! `/api/v1/deviceToken/poll` 得到 `401 User not authenticated`，那正是"还没授权"
//! 的预期响应。也就是说：不是请求写错了，而是**授权这一步在本机根本走不通**。
//!
//! Qoder 客户端**已经登录了**，它把登录态落在
//! `%APPDATA%\com.qodercn.app.stable\auth.v1.dat`。客户端源码（`app.asar`，逐字）：
//!
//! ```text
//! class xte {
//!   constructor(t) { this.filePath = join(t, "auth.v1.dat") }
//!   load() {
//!     if (!existsSync(this.filePath) || !safeStorage.isEncryptionAvailable()) return null;
//!     const t = safeStorage.decryptString(readFileSync(this.filePath));
//!     const A = JSON.parse(t);
//!     return A.schemaVersion !== 1 || typeof A.token !== "string" || ...
//!   }
//! }
//! ```
//!
//! 即 **Electron `safeStorage`**，Windows 上的两段式格式：
//!
//! 1. `Local State` → `os_crypt.encrypted_key` = base64("DPAPI" + DPAPI(AES-256 key))
//! 2. `auth.v1.dat` → `"v10"` + nonce(12) + ciphertext + tag(16)，AES-256-GCM
//!
//! # ★ 我曾误判过一次（记录以免重犯）
//!
//! 我早期说它"是自定义加密、本机拿不到" —— **错的**。我当时拿 DPAPI 的 GUID 头
//! （`01000000d08c9ddf…`）去比 `auth.v1.dat` 的第 3 字节之后，没匹配上就下了结论。
//! 实际上那个头只在**裸 DPAPI** 格式里出现，而 v10 格式的载荷是 **AES-GCM 密文**，
//! DPAPI 只用来保护**密钥**。教训：说"拿不到"之前，先看**客户端自己的源码**怎么读它。
//!
//! # 边界
//!
//! 这是**用户自己机器上、用户自己的**凭证，用户明确要求"直接导入"。密钥由 DPAPI
//! 绑定当前 Windows 用户 —— 它挡的是别的用户，不是本机用户。本模块**只读**，
//! 绝不改动客户端文件。

use serde::Deserialize;

/// 客户端 `auth.v1.dat` 解出来的登录态。
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ClientAuth {
    #[serde(default, rename = "schemaVersion")]
    // Go 侧同名字段也没人读（只在 `ClientAuth` 里落个版本号）。
    // 解析时留着它，是为了"客户端格式换代"这件事在类型上可见。
    #[allow(dead_code)]
    pub(crate) schema_version: i64,
    #[serde(default)]
    pub(crate) token: String,
    #[serde(default, rename = "refreshToken")]
    pub(crate) refresh_token: String,
    #[serde(default, rename = "expiresAt")]
    pub(crate) expires_at: String,
    #[serde(default)]
    pub(crate) user: ClientUser,
}

/// 客户端登录态里的用户信息。
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ClientUser {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) name: String,
    // `email` / `phone` 目前没人读（CLI 只输出 id/name/avatarUrl），
    // 但它们在客户端的登录态里确实存在 —— 留着是为了解析忠实于上游格式，
    // 也方便以后要显示它们时不必再翻一次 DPAPI 解密。
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) email: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) phone: String,
    #[serde(default, rename = "avatarUrl")]
    pub(crate) avatar_url: String,
}

/// 客户端的应用数据目录（候选）。
///
/// 实测本机是 `com.qodercn.app.stable`（CN 稳定版）。国际版/其它渠道的名字不同，
/// 故列多个 —— 只认一个会在别的版本上静默扫不到。
pub(crate) fn app_dirs() -> Vec<String> {
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return Vec::new();
    };
    let base = std::path::PathBuf::from(appdata);
    [
        // 实测（本机）
        "com.qodercn.app.stable",
        // 其它可能的渠道（保守列出，不存在就跳过）
        "com.qoder.app.stable",
        "com.qodercn.app",
        "com.qoder.app",
        "Qoder",
        "QoderCN",
    ]
    .iter()
    .map(|n| base.join(n).to_string_lossy().into_owned())
    .collect()
}

/// 返回第一个**同时有** `Local State` 与 `auth.v1.dat` 的目录。
pub(crate) fn find_client_app_dir() -> Option<String> {
    app_dirs().into_iter().find(|d| {
        let d = std::path::Path::new(d);
        file_exists(&d.join("Local State")) && file_exists(&d.join("auth.v1.dat"))
    })
}

/// 读并解密客户端的登录态（自动探测目录）。
///
/// Go 侧同名的 `ReadClientAuth()`（`clientauth.go:130`）在 Go 仓库里也**没有**
/// 调用点 —— CLI 走的是"先探测目录，再把目录显示给用户"的路径
/// （`runImportClient` 要回报 `clientDir`，故必须先拿到目录）。
/// 保留它是为了与上游 API 表面对齐，也给未来的非 CLI 调用方一个入口。
#[allow(dead_code)]
pub(crate) fn read_client_auth() -> Result<ClientAuth, String> {
    let Some(dir) = find_client_app_dir() else {
        return Err("找不到 Qoder 客户端的登录数据（请先在客户端里登录一次）".to_string());
    };
    read_client_auth_from(&dir)
}

/// 读客户端的机器标识（COSY 签名需要）。
///
/// 客户端把它放在 `auth.machine-id`（实测是本机 UUID）。缺失时返回空串 ——
/// 调用方可以自行生成一个。
pub(crate) fn machine_id(dir: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(dir).join("auth.machine-id"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// 由应用目录推断区域（CN 渠道 vs 国际版）。
///
/// 判据用目录名里的 `cn`：`com.qodercn.app.stable` → CN。猜错会让请求打到错误
/// 端点，故调用方应让用户能改。
pub(crate) fn region_of(dir: &str) -> &'static str {
    let base = std::path::Path::new(dir)
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if base.contains("cn") {
        "cn"
    } else {
        "intl"
    }
}

/// 存在且不是目录（Go `fileExists`）。
fn file_exists(p: &std::path::Path) -> bool {
    std::fs::metadata(p).map(|m| !m.is_dir()).unwrap_or(false)
}

/// 前 `n` 个字节的安全切片（Go `safePrefix`）。
fn safe_prefix(b: &[u8], n: usize) -> String {
    let n = n.min(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

// ---------------------------------------------------------------------------
// 平台分派
// ---------------------------------------------------------------------------

/// 从指定目录读（可测试）。任何一步失败都返回**准确原因**的 error（而不是空结构）——
/// 调用方据此给用户准确的说明，而不是笼统的"读不到"。
#[cfg(windows)]
pub(crate) fn read_client_auth_from(dir: &str) -> Result<ClientAuth, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};

    let dir_path = std::path::Path::new(dir);

    // ---- 1. 取 AES 密钥 ----
    let ls_raw = std::fs::read_to_string(dir_path.join("Local State"))
        .map_err(|e| format!("读 Local State 失败: {e}"))?;
    #[derive(Deserialize)]
    struct LocalState {
        #[serde(default)]
        os_crypt: OsCrypt,
    }
    #[derive(Default, Deserialize)]
    struct OsCrypt {
        #[serde(default)]
        encrypted_key: String,
    }
    let ls: LocalState =
        serde_json::from_str(&ls_raw).map_err(|e| format!("Local State 不是合法 JSON: {e}"))?;
    if ls.os_crypt.encrypted_key.is_empty() {
        return Err("Local State 里没有 os_crypt.encrypted_key（客户端可能还没登录过）".to_string());
    }
    // Electron 用**标准** base64（带 padding），不是 URL-safe。
    let enc_key = base64_std_decode(&ls.os_crypt.encrypted_key)
        .ok_or_else(|| "encrypted_key 不是合法 base64".to_string())?;
    // Electron 在密钥前加了字面量 "DPAPI"
    if enc_key.len() < 5 || &enc_key[..5] != b"DPAPI" {
        return Err(format!(
            "encrypted_key 缺少 DPAPI 前缀（长度 {}）",
            enc_key.len()
        ));
    }
    let aes_key = dpapi_unprotect(&enc_key[5..]).map_err(|e| format!("DPAPI 解出 AES 密钥失败: {e}"))?;
    if aes_key.len() != 32 {
        return Err(format!("AES 密钥长度是 {}（应为 32）", aes_key.len()));
    }

    // ---- 2. 解 auth.v1.dat ----
    let blob = std::fs::read(dir_path.join("auth.v1.dat"))
        .map_err(|e| format!("读 auth.v1.dat 失败: {e}"))?;
    if blob.len() < 3 || &blob[..3] != b"v10" {
        return Err(format!(
            "auth.v1.dat 前缀不是 v10（实际 {:?}）",
            safe_prefix(&blob, 3)
        ));
    }
    let body = &blob[3..];
    if body.len() < 12 + 16 {
        return Err(format!("auth.v1.dat 太短（{} 字节）", body.len()));
    }
    // ★ 布局：nonce(12) ‖ ciphertext ‖ tag(16)。AES-GCM 的 `decrypt` 期望
    // 密文与 tag **拼接**在末尾，故这里先拼好再一次性解 —— 与 Go 的
    // `gcm.Open(nil, nonce, append(ct, tag...), nil)` 完全等价。
    let nonce = &body[..12];
    let ct_and_tag = &body[12..];

    let cipher = Aes256Gcm::new_from_slice(&aes_key)
        .map_err(|e| format!("构造 AES 失败: {e}"))?;
    // 防御：AES-GCM 的密文最短是 16 字节（纯 tag，空明文）。上面已按 12+16 校验过
    // 整体长度，这里再挡一次，避免下游 panic。
    if ct_and_tag.len() < 16 {
        return Err(format!("auth.v1.dat 太短（{} 字节）", ct_and_tag.len()));
    }
    let mut plain = cipher
        .decrypt(Nonce::from_slice(nonce), ct_and_tag)
        .map_err(|e| format!("AES-GCM 解密失败（密钥不匹配或数据损坏）: {e}"))?;

    let auth: ClientAuth = serde_json::from_slice(&plain)
        .map_err(|e| format!("解密结果不是合法 JSON: {e}"))?;
    // 明文里有 token / refreshToken，解析完就擦掉，别让它留在堆上等复用。
    // （只是降低驻留窗口，不是"安全清零" —— Rust 不保证优化器不复制。）
    plain.fill(0);
    if auth.token.is_empty() {
        return Err("解密结果里没有 token 字段".to_string());
    }
    Ok(auth)
}

/// 非 Windows：客户端登录态依赖 DPAPI，**显式报错**而不是静默返回空。
///
/// 声明一个同名可选空依赖会让代码到处 `cfg` 分支，故这里只留一个清晰的失败路径
/// （见 `Cargo.toml` 的 `cfg(not(windows))` 段）。
#[cfg(not(windows))]
pub(crate) fn read_client_auth_from(_dir: &str) -> Result<ClientAuth, String> {
    Err("导入客户端凭证只在 Windows 上可用（客户端登录态由 DPAPI 保护）".to_string())
}

/// 标准 base64（带 padding，可选）。Electron 写的是标准字母表。
fn base64_std_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

// ---------------------------------------------------------------------------
// DPAPI
// ---------------------------------------------------------------------------

/// `CryptProtectData` 的 `CRYPTPROTECT_UI_FORBIDDEN`。
///
/// ★ **必须传**：不传的话 DPAPI 在需要用户交互时会**弹出一个对话框并永远等下去**，
/// 而我们是无人值守的后台进程 —— 那等于挂死。Go 侧同样传了 `0x1`。
#[cfg(windows)]
const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

/// 解密一段由 DPAPI（当前 Windows 用户作用域）保护的数据。
#[cfg(windows)]
fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPT_INTEGER_BLOB,
    };

    if data.is_empty() {
        return Err("DPAPI 输入为空".to_string());
    }
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        // `CryptUnprotectData` 只读入参，但签名要 `*mut u8`；这份 buffer 在本函数
        // 内不会被改动（我们持有 `&[u8]`，cast 只为了让 API 满意）。
        pbData: data.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB::default();

    // SAFETY: `input` 在整个调用期间存活且长度与指针一致；`out` 是本函数的栈变量；
    // entropy / reserved / prompt 三个可选参数按 Go 侧口径传 None（= 空指针）；
    // 出参由系统用 LocalAlloc 分配，下面立刻 LocalFree 释放。
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .map_err(|e| {
            format!("CryptUnprotectData 失败: {e}（该数据可能不属于当前 Windows 用户）")
        })?;
    }

    if out.pbData.is_null() || out.cbData == 0 {
        // 不能直接返回：即便"空"，系统也可能分配了一块内存（`cbData == 0` 时）。
        if !out.pbData.is_null() {
            // SAFETY: 指针来自系统分配，`LocalFree` 是它的配对释放函数。
            unsafe {
                let _ = LocalFree(HLOCAL(out.pbData as *mut core::ffi::c_void));
            }
        }
        return Err("DPAPI 返回了空数据".to_string());
    }

    // SAFETY: 系统保证 `pbData` 指向 `cbData` 个已初始化字节。
    let plain = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    // SAFETY: 同上，指针来自系统分配，只释放一次。
    unsafe {
        let _ = LocalFree(HLOCAL(out.pbData as *mut core::ffi::c_void));
    }
    Ok(plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_dirs_are_under_appdata_and_include_the_verified_name_first() {
        // 顺序有意义：第一个命中的就是选中的那个。
        // ⚠ 这条测试不假设 APPDATA 一定存在（CI/沙箱里可能是空的）。
        let dirs = app_dirs();
        if std::env::var_os("APPDATA").is_none() {
            assert!(dirs.is_empty());
            return;
        }
        assert_eq!(dirs.len(), 6);
        assert!(dirs[0].ends_with("com.qodercn.app.stable"));
        assert!(dirs[1].ends_with("com.qoder.app.stable"));
        for d in &dirs {
            assert!(d.starts_with(&std::env::var("APPDATA").unwrap()));
        }
    }

    #[test]
    fn region_is_decided_by_a_cn_in_the_directory_name() {
        assert_eq!(region_of("C:\\Users\\x\\AppData\\Roaming\\com.qodercn.app.stable"), "cn");
        assert_eq!(region_of("C:\\Users\\x\\AppData\\Roaming\\com.qoder.app.stable"), "intl");
        // 大小写不敏感（Go 侧先 ToLower）。
        assert_eq!(region_of("/home/x/COM.QoderCN.app"), "cn");
    }

    #[test]
    fn safe_prefix_never_panics_on_short_input() {
        assert_eq!(safe_prefix(b"v10abc", 3), "v10");
        assert_eq!(safe_prefix(b"v1", 3), "v1");
        assert_eq!(safe_prefix(b"", 3), "");
    }

    #[test]
    fn standard_base64_is_used_not_url_safe() {
        // Electron 写的是标准字母表（含 `+` / `/` 与 padding）。
        // URL-safe 变体会把这些字符解错或直接失败 —— 钉住用对了哪一个。
        let with_plus_slash = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            [0xfb, 0xff, 0xbf],
        );
        assert_eq!(with_plus_slash, "+/+/");
        assert_eq!(base64_std_decode("+/+/").unwrap(), vec![0xfb, 0xff, 0xbf]);
        assert!(base64_std_decode("not base64!!").is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn reading_client_auth_is_an_explicit_error_off_windows() {
        let err = read_client_auth_from("C:\\whatever").unwrap_err();
        assert!(err.contains("只在 Windows 上可用"), "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn missing_directory_reports_the_first_failing_step() {
        // 目录不存在时，第一步就该报"读 Local State 失败"，而不是笼统的"读不到"。
        let nope = std::env::temp_dir().join("qoder-client-import-does-not-exist");
        let err = read_client_auth_from(&nope.to_string_lossy()).unwrap_err();
        assert!(err.starts_with("读 Local State 失败"), "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn local_state_without_the_key_is_reported_precisely() {
        let dir = std::env::temp_dir().join("qoder-client-import-no-key");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Local State"), r#"{"os_crypt":{}}"#).unwrap();
        let err = read_client_auth_from(&dir.to_string_lossy()).unwrap_err();
        assert!(err.contains("没有 os_crypt.encrypted_key"), "{err}");

        // 不是合法 JSON 时给出另一条更准确的信息。
        std::fs::write(dir.join("Local State"), "not json").unwrap();
        let err = read_client_auth_from(&dir.to_string_lossy()).unwrap_err();
        assert!(err.starts_with("Local State 不是合法 JSON"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn encrypted_key_without_the_dpapi_prefix_is_reported_precisely() {
        let dir = std::env::temp_dir().join("qoder-client-import-bad-prefix");
        std::fs::create_dir_all(&dir).unwrap();
        let key = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            b"NOTDPAPI-and-then-some-padding-bytes",
        );
        std::fs::write(
            dir.join("Local State"),
            format!(r#"{{"os_crypt":{{"encrypted_key":"{key}"}}}}"#),
        )
        .unwrap();
        let err = read_client_auth_from(&dir.to_string_lossy()).unwrap_err();
        assert!(err.contains("缺少 DPAPI 前缀"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn machine_id_is_trimmed_and_missing_is_empty() {
        let dir = std::env::temp_dir().join("qoder-client-import-machine-id");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("auth.machine-id"), "  8b1f-uuid  \n").unwrap();
        assert_eq!(machine_id(&dir.to_string_lossy()), "8b1f-uuid");
        let _ = std::fs::remove_file(dir.join("auth.machine-id"));
        assert_eq!(machine_id(&dir.to_string_lossy()), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_rejects_empty_input_without_calling_the_api() {
        assert_eq!(dpapi_unprotect(&[]).unwrap_err(), "DPAPI 输入为空");
    }
}
