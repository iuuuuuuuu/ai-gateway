//! TRAE 本地登录态捕获（原 `device_proxy.py` `--capture-local` / `capture_from_local` 迁移）。
//!
//! 背景：当前版本 TRAE 的鉴权请求（`api.trae.cn`）不走系统代理，MITM 代理抓不到
//! Cloud-IDE-JWT。但 TRAE 把登录态存在本地 Cookies（Chromium 格式）与 Local Storage
//! leveldb，此处直接解密提取并经 [`update_account_jwt`] 写回账号库，
//! 作为代理方案的兜底（**仅 Windows**：依赖 DPAPI + Chromium Cookies 解密）。
//!
//! 扫描两个应用目录：TRAE SOLO CN（Trae Work）与 Trae CN（Trae IDE），
//! 设置 `TRAE_APP_DIR` 时只扫指定目录。
//!
//! ## 与 [`crate::modules::app_profile`] 的关系
//!
//! 应用数据目录一律经 `app_profile::profile_for(TargetApp::TraeWork / Trae)` 取，
//! 不再自己拼 `%APPDATA%\...` —— 换机器/换安装路径时只需改档案表一处。

use std::path::{Path, PathBuf};

use crate::modules::app_profile::{profile_for, TargetApp};
use crate::modules::config;
use crate::modules::device_proxy::handler::{extract_user_id, update_account_jwt, valid_cloud_ide_jwt, ProxyCtx};
use crate::modules::device_proxy::logger::{ProxyLog, RequestLogger};

/// 候选应用数据目录（对齐 Python `_trae_app_dirs`）：Trae Work + Trae CN；
/// 设置 `TRAE_APP_DIR` 时只扫指定目录（兼容旧环境变量）
pub fn trae_app_dirs() -> Vec<PathBuf> {
    if let Ok(env) = std::env::var("TRAE_APP_DIR") {
        if !env.is_empty() {
            return vec![PathBuf::from(env)];
        }
    }
    let store = config::store_dir();
    vec![
        profile_for(TargetApp::TraeWork, &store).data_dir,
        profile_for(TargetApp::Trae, &store).data_dir,
    ]
}

/// 离线场景构造 [`ProxyCtx`]（复用 handler 的账号写回通路；不启动代理、不发事件）
pub fn offline_ctx() -> ProxyCtx {
    let store = config::store_dir();
    let logs = store.join("logs");
    let captured = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    ProxyCtx {
        log: ProxyLog::new(logs.join("proxy.log"), None, captured),
        req_logger: std::sync::Arc::new(RequestLogger::new(logs)),
        targets: Vec::new(),
        auto_capture_jwt: true,
        data_dir: store,
    }
}

/// 解密 TRAE 本地 Cookies + 扫描 Local Storage leveldb，提取 Cloud-IDE-JWT 写回账号库
///（对齐 Python `capture_from_local`）。返回新增/更新账号数。
#[cfg(windows)]
pub fn capture_from_local() -> Result<serde_json::Value, String> {
    let ctx = offline_ctx();
    let mut total = 0usize;
    for app_dir in trae_app_dirs() {
        match capture_from_app_dir(&ctx, &app_dir) {
            Ok(n) => total += n,
            Err(e) => ctx
                .log
                .log(&format!("[local] 目录 {} 捕获失败: {e}", app_dir.display())),
        }
    }
    ctx.log.log(&format!("[local] 本地捕获完成，新增/更新 {total} 个账号"));
    Ok(serde_json::json!({ "ok": true, "captured": total }))
}

#[cfg(not(windows))]
pub fn capture_from_local() -> Result<serde_json::Value, String> {
    Err("本地捕获仅支持 Windows（需 DPAPI + Chromium Cookies 解密）".into())
}

// ---------------- Windows 解密细节（对齐 Python `_chrome_aes_key`/`_decrypt_cookie`） ----------------

/// 读 `Local State` → `os_crypt.encrypted_key`（DPAPI 包裹的 AES-256 密钥）
#[cfg(windows)]
fn chrome_aes_key(app_dir: &Path) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let lp = app_dir.join("Local State");
    let raw = std::fs::read_to_string(&lp).map_err(|e| format!("Local State 读取失败: {e}"))?;
    let state: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("Local State 解析失败: {e}"))?;
    let b64 = state
        .get("os_crypt")
        .and_then(|v| v.get("encrypted_key"))
        .and_then(serde_json::Value::as_str)
        .ok_or("Local State 无 os_crypt.encrypted_key")?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("encrypted_key base64 解码失败: {e}"))?;
    if raw.len() < 5 || &raw[..5] != b"DPAPI" {
        return Err("encrypted_key 前缀非 DPAPI".into());
    }
    dpapi_unprotect(&raw[5..])
}

/// DPAPI 解密（复用 [`crate::modules::vscode_cn_inject`] 同款 Win32 调用）。
#[cfg(windows)]
fn dpapi_unprotect(encrypted: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
    unsafe {
        let mut data_in = CRYPT_INTEGER_BLOB {
            cbData: encrypted.len() as u32,
            pbData: encrypted.as_ptr() as *mut u8,
        };
        let mut data_out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        CryptUnprotectData(&mut data_in, None, None, None, None, 0, &mut data_out)
            .map_err(|e| format!("DPAPI CryptUnprotectData 失败: {e}"))?;
        if data_out.pbData.is_null() || data_out.cbData == 0 {
            return Err("DPAPI 返回空数据".to_string());
        }
        let slice = std::slice::from_raw_parts(data_out.pbData, data_out.cbData as usize);
        let result = slice.to_vec();
        let _ = LocalFree(HLOCAL(data_out.pbData as _));
        Ok(result)
    }
}

/// 单个 cookie 密文解密：`v10` 为 AES-256-GCM（`'v10'` + nonce12 + ct + tag16），
/// 旧格式直接 DPAPI。key 缺失时 v10 放弃（返回 `None`，调用方回退明文 value）。
#[cfg(windows)]
fn decrypt_cookie(enc: &[u8], key: Option<&[u8]>) -> Option<String> {
    if enc.len() >= 3 && &enc[..3] == b"v10" {
        use aes_gcm::aead::Aead;
        use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
        if enc.len() < 19 {
            return None;
        }
        let key = key?;
        let cipher = Aes256Gcm::new_from_slice(key).ok()?;
        let plain = cipher.decrypt(Nonce::from_slice(&enc[3..15]), &enc[15..]).ok()?;
        return Some(String::from_utf8_lossy(&plain).to_string());
    }
    dpapi_unprotect(enc).ok().map(|v| String::from_utf8_lossy(&v).to_string())
}

/// 从一段文本里找 Cloud-IDE-JWT（对齐 Python `_find_cloud_ide_jwt`）：
/// 先匹配显式前缀，命中即返回（无效也直接 `None`，不落入通用扫描）；
/// 否则通用三段 JWT 逐个校验，返回首个通过者（格式化为 `Cloud-IDE-JWT <jwt>`）。
pub fn find_cloud_ide_jwt(blob: &str) -> Option<String> {
    use std::sync::OnceLock;
    static PREFIX_RE: OnceLock<regex::Regex> = OnceLock::new();
    static JWT_RE: OnceLock<regex::Regex> = OnceLock::new();
    if blob.is_empty() {
        return None;
    }
    let prefix = PREFIX_RE.get_or_init(|| {
        regex::Regex::new(r"Cloud-IDE-JWT\s+([A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+)")
            .expect("jwt prefix regex")
    });
    if let Some(caps) = prefix.captures(blob) {
        return valid_cloud_ide_jwt(&caps[1]);
    }
    let jwt = JWT_RE.get_or_init(|| {
        regex::Regex::new(r"[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+").expect("jwt regex")
    });
    for m in jwt.find_iter(blob) {
        if let Some(v) = valid_cloud_ide_jwt(m.as_str()) {
            return Some(v);
        }
    }
    None
}

/// 对单个应用数据目录执行 Cookies 解密 + leveldb 明文扫描（对齐 Python
/// `_capture_from_app_dir`）。返回新增/更新账号数。
#[cfg(windows)]
fn capture_from_app_dir(ctx: &ProxyCtx, app_dir: &Path) -> Result<usize, String> {
    let mut found = 0usize;
    ctx.log.log(&format!("[local] TRAE 数据目录: {}", app_dir.display()));
    if !app_dir.is_dir() {
        ctx.log.log("[local] 目录不存在，跳过");
        return Ok(0);
    }
    let key = match chrome_aes_key(app_dir) {
        Ok(k) => Some(k),
        Err(e) => {
            ctx.log.log(&format!("[local] 未取得 AES 密钥({e})；将仅扫描明文值"));
            None
        }
    };

    // 1) Cookies 数据库：主分区 + trae-webview 分区
    let mut dbs = vec![app_dir.join("Network").join("Cookies")];
    let tw = app_dir.join("Partitions").join("trae-webview").join("Cookies");
    if tw.exists() {
        dbs.push(tw);
    }
    for db in dbs {
        if !db.exists() {
            continue;
        }
        match scan_cookies_db(ctx, &db, key.as_deref()) {
            Ok(n) => found += n,
            Err(e) => ctx.log.log(&format!("[local] 读取 Cookies 失败 {}: {e}", db.display())),
        }
    }

    // 2) Local Storage leveldb 明文兜底扫描
    for ls in [
        app_dir.join("Local Storage").join("leveldb"),
        app_dir
            .join("Partitions")
            .join("trae-webview")
            .join("Local Storage")
            .join("leveldb"),
    ] {
        if !ls.is_dir() {
            continue;
        }
        found += scan_leveldb_dir(ctx, &ls);
    }
    Ok(found)
}

/// 复制 Cookies 库到临时目录后只读打开（规避客户端运行时文件锁；
/// `-wal`/`-shm` 一并复制避免读到未 checkpoint 的空库），返回命中的账号数
#[cfg(windows)]
fn scan_cookies_db(ctx: &ProxyCtx, db: &Path, key: Option<&[u8]>) -> Result<usize, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let td = std::env::temp_dir().join(format!("aigw_trae_ck_{}_{nanos}", std::process::id()));
    std::fs::create_dir_all(&td).map_err(|e| format!("临时目录创建失败: {e}"))?;
    let tmp_db = td.join("Cookies");
    let copy_res = std::fs::copy(db, &tmp_db).map_err(|e| format!("Cookies 复制失败: {e}"));
    if copy_res.is_err() {
        let _ = std::fs::remove_dir_all(&td);
        return Err(copy_res.unwrap_err());
    }
    for suffix in ["-wal", "-shm"] {
        let side = db.with_file_name(format!("Cookies{suffix}"));
        if side.exists() {
            let _ = std::fs::copy(&side, td.join(format!("Cookies{suffix}")));
        }
    }

    let result = (|| -> Result<usize, String> {
        let conn = rusqlite::Connection::open_with_flags(
            &tmp_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| format!("打开 Cookies 库失败: {e}"))?;
        let mut stmt = conn
            .prepare("SELECT host_key, name, value, encrypted_value FROM cookies")
            .map_err(|e| format!("查询 cookies 表失败: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })
            .map_err(|e| format!("遍历 cookies 失败: {e}"))?;
        let mut found = 0usize;
        for r in rows.flatten() {
            let (host, name, value, enc) = r;
            // 明文 value 优先作 blob；密文解密成功则覆盖（解密失败回退明文，对齐 Python）
            let mut blob = value;
            if !enc.is_empty() {
                if let Some(d) = decrypt_cookie(&enc, key) {
                    blob = d;
                }
            }
            let Some(hit) = find_cloud_ide_jwt(&blob) else { continue };
            let Some(uid) = extract_user_id(&hit) else { continue };
            let status = update_account_jwt(ctx, &uid, &hit);
            ctx.log.log(&format!("[local] 命中 Cookies host={host} name={name} -> {status}"));
            found += 1;
        }
        Ok(found)
    })();
    let _ = std::fs::remove_dir_all(&td);
    result
}

/// leveldb 目录明文扫描：`.ldb`/`.log` 文件按字节正则找 `Cloud-IDE-JWT <jwt>`
///（对齐 Python：不做结构化解析，命中即写回）
#[cfg(windows)]
fn scan_leveldb_dir(ctx: &ProxyCtx, ls: &Path) -> usize {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::bytes::Regex::new(r"Cloud-IDE-JWT [A-Za-z0-9_\-\.=]+").expect("leveldb jwt regex")
    });
    let mut found = 0usize;
    let Ok(entries) = std::fs::read_dir(ls) else { return 0 };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.ends_with(".ldb") || name.ends_with(".log")) {
            continue;
        }
        let Ok(data) = std::fs::read(entry.path()) else { continue };
        for m in re.find_iter(&data) {
            let hit = String::from_utf8_lossy(m.as_bytes()).to_string();
            let Some(uid) = extract_user_id(&hit) else { continue };
            let status = update_account_jwt(ctx, &uid, &hit);
            ctx.log.log(&format!("[local] 命中 leveldb {name} -> {status}"));
            found += 1;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::config::test_isolation::Isolated;

    /// `find_cloud_ide_jwt`：显式前缀优先（无效即止）+ 通用 JWT 校验回退
    #[test]
    fn find_jwt_prefix_and_generic() {
        use base64::Engine;
        let b64 = |v: &serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
        };
        let header = b64(&serde_json::json!({"alg": "RS256"}));
        let payload = b64(&serde_json::json!({"data": {"id": "u1"}}));
        let jwt = format!("{header}.{payload}.sig");
        // 显式前缀
        let hit = find_cloud_ide_jwt(&format!("blob Cloud-IDE-JWT {jwt} tail")).unwrap();
        assert_eq!(hit, format!("Cloud-IDE-JWT {jwt}"));
        // 通用扫描（无前缀）
        let hit2 = find_cloud_ide_jwt(&format!("x {jwt} y")).unwrap();
        assert_eq!(hit2, format!("Cloud-IDE-JWT {jwt}"));
        // 前缀命中但无效（HS256）→ 直接 None，不落入通用扫描
        let h2 = b64(&serde_json::json!({"alg": "HS256"}));
        let bad = format!("Cloud-IDE-JWT {h2}.{payload}.sig");
        assert!(find_cloud_ide_jwt(&bad).is_none());
        // 空串
        assert!(find_cloud_ide_jwt("").is_none());
    }

    /// 应用数据目录来自档案表（不是硬编码 %APPDATA% 拼接）
    #[test]
    fn app_dirs_come_from_profile_table() {
        let _iso = Isolated::new("local-capture-dirs");
        let dirs = trae_app_dirs();
        assert_eq!(dirs.len(), 2);
        let store = config::store_dir();
        assert_eq!(dirs[0], profile_for(TargetApp::TraeWork, &store).data_dir);
        assert_eq!(dirs[1], profile_for(TargetApp::Trae, &store).data_dir);
        assert_ne!(dirs[0], dirs[1], "两个 Trae 应用目录必须分开");
    }

    /// TRAE_APP_DIR 覆盖时只扫指定目录（兼容旧环境变量）
    #[test]
    fn trae_app_dir_env_overrides() {
        let _iso = Isolated::new("local-capture-env");
        std::env::set_var("TRAE_APP_DIR", r"D:\fake\trae");
        let dirs = trae_app_dirs();
        std::env::remove_var("TRAE_APP_DIR");
        assert_eq!(dirs, vec![PathBuf::from(r"D:\fake\trae")]);
    }

    /// 离线 ctx 的路径全部落在 store_dir 下
    #[test]
    fn offline_ctx_uses_store_dir() {
        let iso = Isolated::new("local-capture-ctx");
        let ctx = offline_ctx();
        assert_eq!(ctx.data_dir, iso.dir());
        assert!(ctx.targets.is_empty(), "离线捕获不需要域名列表");
        assert!(ctx.auto_capture_jwt);
    }

    /// 目录不存在时静默返回 0（不报错）
    #[cfg(windows)]
    #[test]
    fn missing_app_dir_returns_zero() {
        let _iso = Isolated::new("local-capture-missing");
        let ctx = offline_ctx();
        let missing = std::env::temp_dir().join("aigw_definitely_missing_dir_12345");
        assert_eq!(capture_from_app_dir(&ctx, &missing).unwrap(), 0);
    }

    /// 非 Windows：本地捕获显式报错（依赖 DPAPI）
    #[cfg(not(windows))]
    #[test]
    fn capture_is_unsupported_off_windows() {
        assert!(capture_from_local().is_err());
    }
}
